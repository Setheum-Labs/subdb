use std::path::PathBuf;
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use memmap::{MmapMut, MmapOptions};
use parity_scale_codec::{self as codec, Encode, Decode, Compact};
use crate::types::{KeyType, SimpleWriter};
use crate::datum_size::DatumSize;

/// How many references a storage table item has.
pub type RefCount = u16;

/// Where in a storage table an item is.
pub type TableItemIndex = u16;

/// How many table items; must be able to store a range from 0 to TableItemIndex::max_value() + 1
/// inclusive, therefore needs the next biggest type up.
pub type TableItemCount = u32;

pub struct Table<K> {
	file: std::fs::File,
	data: MmapMut,
	header_data: MmapMut,
	item_header_size: usize,
	item_size: usize,
	item_count: TableItemCount,
	value_size: usize,
	_dummy: std::marker::PhantomData<K>,
}

/// Rather unsafe.
#[derive(Clone, Copy)]
struct TableHeader {
	/// The number of items used. Never more than `touched_count`.
	used: TableItemCount,
	/// Ignore if used == touched_count; otherwise it is the next free item.
	next_free: TableItemIndex,
	/// The number of unique slots that have been allocated at some point. Never more than
	/// `item_count`.
	///
	/// Item indices equal to this and less than `item_count` may be allocated in addition to the
	/// linked list starting at `next_free`.
	touched_count: TableItemCount,
}

impl TableHeader {
	fn next_free(&self, item_count: TableItemCount) -> Option<TableItemIndex> {
		if self.used < self.touched_count {
			Some(self.next_free as TableItemIndex)
		} else {
			if self.touched_count < item_count {
				Some(self.touched_count as u16)
			} else {
				None
			}
		}
	}
}

#[derive(Clone, Debug)]
enum ItemHeader<K: Encode + Decode> {
	Allocated {
		/// Number of times this item has been inserted, without a corresponding remove, into the
		/// database.
		ref_count: RefCount,
		size_correction: u32,
		key: K,
	},
	Free(
		/// If `used < touched_count`, then the next free item's index. If the two are equal, then
		/// this is undefined.
		TableItemIndex,
	),
}

impl<K: Encode + Decode> ItemHeader<K> {
	fn as_next_free(&self) -> TableItemIndex {
		match self {
			ItemHeader::Free(next_free) => *next_free,
			ItemHeader::Allocated {..} => panic!("Free expected. Database corruption?"),
		}
	}

	fn as_size_correction(&self) -> usize {
		match self {
			ItemHeader::Allocated { size_correction, .. } => *size_correction as usize,
			ItemHeader::Free(_) => panic!("Allocated expected. Database corruption?"),
		}
	}
}

// TODO: Two smaller ItemHeader Encode/Decode shim impls to be used depending on size range of this
//       table's entries.
//       Smaller ranges need only u8 or u16 size_corrections.
impl<K: Encode + Decode> Decode for ItemHeader<K> {
	fn decode<I: codec::Input>(input: &mut I) -> Result<Self, codec::Error> {
		let ref_count = RefCount::decode(input)?;
		let size_correction = u32::decode(input)?.into();
		Ok(if ref_count > 0 {
			Self::Allocated { ref_count, size_correction, key: K::decode(input)? }
		} else {
			Self::Free(TableItemIndex::decode(input)?)
		})
	}
}

impl<K: Encode + Decode> Encode for ItemHeader<K> {
	fn encode_to<O: codec::Output>(&self, output: &mut O) {
		match self {
			ItemHeader::Allocated { ref_count, size_correction, key} => {
				assert!(*ref_count > 0);
				(ref_count, size_correction, key).encode_to(output);
			}
			ItemHeader::Free(index) => {
				(RefCount::default(), index).encode_to(output);
			}
		}
	}
}

impl<K: KeyType> Table<K> {
	pub fn open(path: PathBuf, datum_size: DatumSize) -> Self {
		assert!(!path.exists() || path.is_file(), "Path must either not exist or be a file.");

		let file = OpenOptions::new()
			.read(true)
			.write(true)
			.create(true)
			.open(&path)
			.expect("Path must be writable.");
		let len = file.metadata().expect("File must be readable").len();
		let value_size = datum_size.size().expect("Data must be sized");
		let item_count = datum_size.contents_entries() as TableItemCount;
		let item_header_size = size_of::<RefCount>() + size_of::<u32>() + K::SIZE.max(size_of::<TableItemIndex>());
		let item_size = value_size + item_header_size;
		let table_header_size = size_of::<TableHeader>();
		let total_size = table_header_size + item_size * item_count as usize;

		assert!(len == 0 || len == total_size as u64, "File exists but length is unexpected");
		file.set_len(total_size as u64).expect("Path must be writable.");

		let header_data = unsafe {
			MmapOptions::new()
				.len(table_header_size)
				.map_mut(&file)
				.expect("Path must be writable.")
		};
		let data = unsafe {
			MmapOptions::new()
				.offset(table_header_size as u64)
				.map_mut(&file)
				.expect("Path must be writable.")
		};

		Self { file, data, header_data, item_count, item_size, item_header_size, value_size, _dummy: Default::default() }
	}

	fn header_mut(&mut self) -> &mut TableHeader {
		unsafe {
			let (pre, header, post) = self.header_data.align_to_mut::<TableHeader>();
			assert!(pre.is_empty());
			assert!(post.is_empty());
			&mut header[0]
		}
	}

	fn header(&self) -> &TableHeader {
		unsafe {
			let (pre, header, post) = self.header_data.align_to::<TableHeader>();
			assert!(pre.is_empty());
			assert!(post.is_empty());
			&header[0]
		}
	}

	fn set_header(&mut self, h: TableHeader) {
		*self.header_mut() = h;
	}

	fn mutate_item_header<R>(&mut self, i: TableItemIndex, f: impl FnOnce(&mut ItemHeader<K>) -> R) -> R {
		let data = &mut self.data[
			self.item_size * i as usize..self.item_size * i as usize + self.item_header_size
		];
		let mut h = ItemHeader::decode(&mut &data[..]).expect("Database corrupt?");
		let r = f(&mut h);
		h.encode_to(&mut SimpleWriter(data, 0));
		r
	}

	fn item_header(&self, i: TableItemIndex) -> ItemHeader<K> {
		let data = &self.data[
			self.item_size * i as usize..self.item_size * i as usize + self.item_header_size
		];
		ItemHeader::decode(&mut &data[..]).expect("Database corrupt?")
	}

	/// Attempt to find a free slot (but do not allocate).
	pub fn next_free(&self) -> Option<TableItemIndex> {
		self.header().next_free(self.item_count)
	}

	/// Retrieve a table item's data as an immutable pointer.
	pub fn item_ref_count(&self, i: TableItemIndex) -> RefCount {
		match self.item_header(i) {
			ItemHeader::Free(_) => 0,
			ItemHeader::Allocated { ref_count, .. } => ref_count,
		}
	}

	/// Retrieve a table item's data as an immutable pointer.
	pub fn item_ref(&self, i: TableItemIndex) -> &[u8] {
		let size = self.value_size - self.item_header(i).as_size_correction();
		let p = self.item_size * i as usize + self.item_header_size;
		&self.data[p..p + size]
	}

	/// Retrieve a table item's data as a mutable pointer.
	pub fn item_mut(&mut self, i: TableItemIndex) -> &mut [u8] {
		let size = self.value_size - self.item_header(i).as_size_correction();
		let p = self.item_size * i as usize + self.item_header_size;
		&mut self.data[p..p + size]
	}

	/// Add another reference to a slot that is already allocated and return the resulting number of
	/// references. Err if the slot is not allocated.
	pub fn reference(&mut self, i: TableItemIndex) -> Result<RefCount, ()> {
		self.mutate_item_header(i, |item| {
			match item {
				ItemHeader::Allocated { ref mut ref_count, .. } => {
					*ref_count += 1;
					Ok(*ref_count)
				}
				ItemHeader::Free(..) => Err(()),
			}
		})
	}

	/// Attempt to allocate a slot.
	pub fn allocate(&mut self, key: &K, size: usize) -> Option<TableItemIndex> {
		let mut h = self.header().clone();
		let size_correction = (self.value_size - size) as u32;
		// OPTIMISE: Avoid extra copy of `key` by writing directly to map.
		let new_item = ItemHeader::Allocated { ref_count: 1, size_correction, key: key.clone() };
		let result = if h.used < h.touched_count {
			let result = h.next_free;
			let new_next_free = self.mutate_item_header(result, |item| {
				let new_next_free = item.as_next_free();
				*item = new_item;
				new_next_free
			});
			h.next_free = new_next_free;
			h.used += 1;
			self.set_header(h);
			result
		} else {
			if h.touched_count < self.item_count {
				let result = h.touched_count as TableItemIndex;
				h.touched_count += 1;
				h.used += 1;
				self.mutate_item_header(result, |item| {
					assert!(matches!(item, ItemHeader::Free(_)), "Free slot expected. Database corrupt?");
					dbg!(&new_item);
					*item = new_item;
				});
				result
			} else {
				return None
			}
		};
		Some(result)
	}

	/// Free up a slot or decrease the reference count if it's greater than 1. Returns Ok along with
	/// the number of refs remaining, or Err if the slot was already free.
	pub fn free(&mut self, i: TableItemIndex) -> Result<RefCount, ()> {
		let mut h = *self.header();
		let result = self.mutate_item_header(i, |item| {
			match item {
				ItemHeader::Allocated { ref mut ref_count, .. } => {
					assert!(*ref_count > 0, "Database corrupt? Zero refs.");
					if *ref_count > 1 {
						*ref_count -= 1;
						return Ok(*ref_count)
					}
				}
				ItemHeader::Free(..) => return Err(()),
			}
			// Stich the old free list head onto this item.
			*item = ItemHeader::Free(h.next_free);
			Ok(0)
		})?;
		if result == 0 {
			// Add the item to the free list.
			h.used = h.used.checked_sub(1)
				.expect("Database corrupt? used count underflow");
			h.next_free = i;
			self.set_header(h);
		}
		Ok(result)
	}

	/// The amount of slots left in this table.
	pub fn available(&self) -> TableItemCount {
		self.item_count - self.header().used
	}
}
