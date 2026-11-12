// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Packed layouts of the fixed-shape perf_event_open ring-buffer records
//! perfr consumes. The kernel's perf ABI (`linux/perf_event.h`) is the spec.
//!
//! The `record!` macro below generates each struct, its parser, and its
//! field-offset table from one field list, so those three cannot drift from
//! each other; the tests pin the sizes the ABI fixes. Samples are not here:
//! their shape depends on `sample_type`, and [`reader`] walks them by flag.

#![deny(unsafe_code)]

pub mod reader;
pub mod tracepoints;

/// Little-endian decoding of one field out of a byte slice. The slice handed
/// to `from_le_slice` is exactly `SIZE` bytes.
pub trait FromLe: Sized + Copy {
    const SIZE: usize;
    fn from_le_slice(bytes: &[u8]) -> Self;
}

macro_rules! impl_from_le_int {
    ($($ty:ty),+) => {
        $(impl FromLe for $ty {
            const SIZE: usize = core::mem::size_of::<$ty>();
            fn from_le_slice(bytes: &[u8]) -> Self {
                Self::from_le_bytes(bytes.try_into().expect("caller sliced SIZE bytes"))
            }
        })+
    };
}
impl_from_le_int!(u16, u32, u64);

impl<T: FromLe, const N: usize> FromLe for [T; N] {
    const SIZE: usize = T::SIZE * N;
    fn from_le_slice(bytes: &[u8]) -> Self {
        core::array::from_fn(|i| T::from_le_slice(&bytes[i * T::SIZE..(i + 1) * T::SIZE]))
    }
}

/// Defines a packed record struct plus its parser and field-offset table
/// from a single field list. Fields decode in declaration order; because the
/// struct is `repr(C, packed)`, declaration order and byte order coincide,
/// which `parse` debug-asserts by walking exactly `SIZE` bytes.
macro_rules! record {
    ($(#[$meta:meta])* $name:ident { $($field:ident: $ty:ty),+ $(,)? }) => {
        $(#[$meta])*
        #[repr(C, packed)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name {
            $(pub $field: $ty,)+
        }

        impl $name {
            pub const SIZE: usize = core::mem::size_of::<Self>();

            /// Byte offset of each field, in declaration order.
            pub const FIELD_OFFSETS: &'static [usize] = &[
                $(core::mem::offset_of!($name, $field),)+
            ];

            pub fn parse(bytes: &[u8]) -> Option<Self> {
                if bytes.len() < Self::SIZE {
                    return None;
                }
                let mut offset = 0usize;
                $(
                    let $field =
                        <$ty as FromLe>::from_le_slice(&bytes[offset..offset + <$ty as FromLe>::SIZE]);
                    offset += <$ty as FromLe>::SIZE;
                )+
                debug_assert_eq!(offset, Self::SIZE);
                Some(Self { $($field,)+ })
            }
        }

        impl FromLe for $name {
            const SIZE: usize = core::mem::size_of::<Self>();
            fn from_le_slice(bytes: &[u8]) -> Self {
                Self::parse(bytes).expect("caller sliced SIZE bytes")
            }
        }
    };
}

/// The record type ids of `perf_event_header::type` perfr consumes, from
/// `linux/perf_event.h`.
pub mod record_type {
    pub const MMAP: u32 = 1;
    pub const LOST: u32 = 2;
    pub const EXIT: u32 = 4;
    pub const FORK: u32 = 7;
    pub const SAMPLE: u32 = 9;
    pub const SWITCH: u32 = 14;
    pub const SWITCH_CPU_WIDE: u32 = 15;
}

record!(
    /// `perf_event_header`. The kernel calls the first field `type`, a
    /// Rust keyword, so it is `kind` here; the layout is what matters.
    PerfEventHeader {
        kind: u32,
        misc: u16,
        size: u16,
    }
);

record!(
    /// Must stay in sync with `kSampleTypeTidTimeStreamidCpu`: the bits set
    /// in `perf_event_attr::sample_type` determine these fields.
    SampleIdTidTimeStreamidCpu {
        pid: u32,
        tid: u32,
        time: u64,
        stream_id: u64,
        cpu: u32,
        res: u32,
    }
);

record!(
    ForkExit {
        header: PerfEventHeader,
        pid: u32,
        ppid: u32,
        tid: u32,
        ptid: u32,
        time: u64,
        sample_id: SampleIdTidTimeStreamidCpu,
    }
);

record!(
    /// The record continues with `char filename[]` and the sample id, both
    /// read dynamically because `filename` is variable-length.
    MmapUpToPgoff {
        header: PerfEventHeader,
        pid: u32,
        tid: u32,
        address: u64,
        length: u64,
        page_offset: u64,
    }
);

record!(
    Lost {
        header: PerfEventHeader,
        id: u64,
        lost: u64,
        sample_id: SampleIdTidTimeStreamidCpu,
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_the_perf_abi() {
        assert_eq!(PerfEventHeader::SIZE, 8);
        assert_eq!(SampleIdTidTimeStreamidCpu::SIZE, 32);
        assert_eq!(ForkExit::SIZE, 64);
        assert_eq!(MmapUpToPgoff::SIZE, 40);
        assert_eq!(Lost::SIZE, 56);
    }

    #[test]
    fn offsets_are_dense_and_end_at_size() {
        fn check(offsets: &[usize], sizes: &[usize], total: usize) {
            let mut expected = 0usize;
            for (offset, size) in offsets.iter().zip(sizes) {
                assert_eq!(*offset, expected);
                expected += size;
            }
            assert_eq!(expected, total);
        }
        check(
            ForkExit::FIELD_OFFSETS,
            &[8, 4, 4, 4, 4, 8, 32],
            ForkExit::SIZE,
        );
        check(Lost::FIELD_OFFSETS, &[8, 8, 8, 32], Lost::SIZE);
        check(
            MmapUpToPgoff::FIELD_OFFSETS,
            &[8, 4, 4, 8, 8, 8],
            MmapUpToPgoff::SIZE,
        );
    }

    #[test]
    fn fork_exit_parses_field_by_field() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&record_type::FORK.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(ForkExit::SIZE as u16).to_le_bytes());
        for value in [11u32, 10, 12, 10] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&700u64.to_le_bytes());
        bytes.extend_from_slice(&11u32.to_le_bytes());
        bytes.extend_from_slice(&12u32.to_le_bytes());
        bytes.extend_from_slice(&700u64.to_le_bytes());
        bytes.extend_from_slice(&3u64.to_le_bytes());
        bytes.extend_from_slice(&5u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let record = ForkExit::parse(&bytes).unwrap();
        assert_eq!({ record.header.kind }, record_type::FORK);
        assert_eq!({ record.header.size } as usize, ForkExit::SIZE);
        assert_eq!({ record.pid }, 11);
        assert_eq!({ record.ppid }, 10);
        assert_eq!({ record.tid }, 12);
        assert_eq!({ record.ptid }, 10);
        assert_eq!({ record.time }, 700);
        assert_eq!({ record.sample_id.stream_id }, 3);
        assert_eq!({ record.sample_id.cpu }, 5);

        assert!(ForkExit::parse(&bytes[..ForkExit::SIZE - 1]).is_none());
    }

    #[test]
    fn nested_parse_round_trips() {
        let mut bytes = vec![0u8; Lost::SIZE];
        bytes[0..4].copy_from_slice(&record_type::LOST.to_le_bytes());
        bytes[8..16].copy_from_slice(&77u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&1234u64.to_le_bytes());
        let record = Lost::parse(&bytes).unwrap();
        assert_eq!({ record.header.kind }, record_type::LOST);
        assert_eq!({ record.id }, 77);
        assert_eq!({ record.lost }, 1234);
    }
}
