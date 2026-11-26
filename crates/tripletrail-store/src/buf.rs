//! [`Buf`]: an array that is either owned or a view into a memory-mapped checkpoint file.
//!
//! A store opened read-only maps its checkpoint files instead of reading them: the pages are
//! shared with the page cache (and with every other process that maps the same files), are
//! read on first touch, and can be dropped by the kernel under memory pressure. Writes never
//! reach a mapped array: [`Buf::to_mut`] copies it first.

use memmap2::Mmap;
use std::ops::Deref;
use std::sync::Arc;

/// Plain values a file region can be viewed as: any bit pattern is a valid value.
///
/// # Safety
///
/// Implementors must have no padding, no invalid bit patterns and no drop glue.
pub(crate) unsafe trait Pod: Copy + 'static {}

// SAFETY: integers and arrays of integers have no padding and accept every bit pattern.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}
// SAFETY: as above.
unsafe impl Pod for [u64; 4] {}

pub(crate) enum Buf<T: Pod> {
    Owned(Vec<T>),
    /// Values on the heap that another array shares (see [`Buf::share`]).
    Shared(Arc<Vec<T>>),
    /// `len` values starting at byte `offset` of `map`, which is suitably aligned.
    Mapped {
        map: Arc<Mmap>,
        offset: usize,
        len: usize,
    },
}

impl<T: Pod> Default for Buf<T> {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl<T: Pod> From<Vec<T>> for Buf<T> {
    fn from(v: Vec<T>) -> Self {
        Self::Owned(v)
    }
}

impl<T: Pod> Clone for Buf<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Owned(v) => Self::Owned(v.clone()),
            Self::Shared(v) => Self::Shared(Arc::clone(v)),
            Self::Mapped { map, offset, len } => Self::Mapped {
                map: Arc::clone(map),
                offset: *offset,
                len: *len,
            },
        }
    }
}

impl<T: Pod> Deref for Buf<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Self::Owned(v) => v,
            Self::Shared(v) => v,
            Self::Mapped { map, offset, len } => {
                // SAFETY: `mapped` checked that the range is inside the map and aligned for
                // `T`, and `T: Pod` accepts any bytes. The map is read-only and lives as long
                // as `self`.
                unsafe { std::slice::from_raw_parts(map.as_ptr().add(*offset).cast::<T>(), *len) }
            }
        }
    }
}

impl<T: Pod> Buf<T> {
    /// A view of `len` values at byte `offset` of `map`, or a copy when the region is not
    /// aligned for `T` or the machine is big-endian (files are little-endian). `None` if the
    /// region runs past the end of the file.
    pub(crate) fn mapped(map: &Arc<Mmap>, offset: usize, len: usize) -> Option<Self> {
        let size = std::mem::size_of::<T>();
        let end = offset.checked_add(len.checked_mul(size)?)?;
        if end > map.len() {
            return None;
        }
        let aligned = (map.as_ptr() as usize + offset).is_multiple_of(std::mem::align_of::<T>());
        if aligned && (cfg!(target_endian = "little") || size == 1) {
            return Some(Self::Mapped {
                map: Arc::clone(map),
                offset,
                len,
            });
        }
        // Rare: copy (and convert from little-endian) value by value.
        let bytes = &map[offset..end];
        let mut out = Vec::with_capacity(len);
        for chunk in bytes.chunks_exact(size) {
            // SAFETY: `chunk` holds exactly `size_of::<T>()` bytes and `T: Pod`.
            let mut v: T = unsafe { std::ptr::read_unaligned(chunk.as_ptr().cast::<T>()) };
            if cfg!(target_endian = "big") && size > 1 {
                // SAFETY: reverses the bytes of each 8- or 4-byte word of a plain value.
                let raw =
                    unsafe { std::slice::from_raw_parts_mut((&raw mut v).cast::<u8>(), size) };
                let word = if size.is_multiple_of(8) { 8 } else { 4 };
                for w in raw.chunks_exact_mut(word) {
                    w.reverse();
                }
            }
            out.push(v);
        }
        Some(Self::Owned(out))
    }

    /// The values as a vector to change, copying them out of the map first.
    pub(crate) fn to_mut(&mut self) -> &mut Vec<T> {
        match self {
            Self::Mapped { .. } => *self = Self::Owned(self.to_vec()),
            Self::Shared(v) => *self = Self::Owned(Arc::unwrap_or_clone(std::mem::take(v))),
            Self::Owned(_) => {}
        }
        match self {
            Self::Owned(v) => v,
            _ => unreachable!(),
        }
    }

    /// A second array of the same values, without copying them: values on the heap are
    /// shared from now on (`self` too becomes [`Buf::Shared`]).
    pub(crate) fn share(&mut self) -> Self {
        if let Self::Owned(v) = self {
            *self = Self::Shared(Arc::new(std::mem::take(v)));
        }
        self.clone()
    }

    /// Bytes held on the heap (mapped values are not).
    pub(crate) fn heap_size(&self) -> usize {
        match self {
            Self::Owned(v) => v.capacity() * std::mem::size_of::<T>(),
            // Divided among the arrays that share them.
            Self::Shared(v) => {
                v.capacity() * std::mem::size_of::<T>() / Arc::strong_count(v).max(1)
            }
            Self::Mapped { .. } => 0,
        }
    }

    /// For mapped values, read ahead in order (`in_order`), or only the page asked for
    /// (see `Spill::map_keys`).
    pub(crate) fn advise_in_order(&self, in_order: bool) {
        #[cfg(unix)]
        if let Self::Mapped { map, offset, len } = self {
            let advice = if in_order {
                memmap2::Advice::Sequential
            } else {
                memmap2::Advice::Random
            };
            let _ = map.advise_range(advice, *offset, len * std::mem::size_of::<T>());
        }
        #[cfg(not(unix))]
        let _ = in_order;
    }

    /// Bytes of the file this array is mapped from (0 when owned).
    pub(crate) fn mapped_size(&self) -> usize {
        match self {
            Self::Owned(_) | Self::Shared(_) => 0,
            Self::Mapped { len, .. } => len * std::mem::size_of::<T>(),
        }
    }
}
