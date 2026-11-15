// Copyright (c) 2026 The perfr Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Files built byte by byte ([`crate::testing`]), so that each test says
//! which part of the format it is about.

use super::*;
use crate::reader::sample_type::*;
use crate::testing::*;

/// One process with a main thread and a worker, sampled with call chains.
/// The worker's ring is written after the main thread's, as `perf` writes
/// one CPU after another, so the file is not in time order.
fn two_thread_file() -> Vec<u8> {
    let e = Event1 {
        sample_type: CALLCHAIN_SAMPLE,
        id: 7,
    };
    let records = vec![
        e.kernel_mmap(0xffff_ffff_8000_0000, 0x100_0000, "[kernel.kallsyms]_text"),
        e.comm(10, 10, "app", true, 100),
        e.mmap2(10, 0x40_0000, 0x1000, 0x1000, "/opt/app", 110),
        e.sample(
            10,
            10,
            300,
            0x40_0010,
            &[CONTEXT_USER, 0x40_0010, 0x40_0200],
        ),
        e.sample(
            10,
            10,
            500,
            0xffff_ffff_8000_0123,
            &[
                CONTEXT_KERNEL,
                0xffff_ffff_8000_0123,
                CONTEXT_USER,
                0x40_0020,
                0x40_0200,
            ],
        ),
        // The other ring: earlier events, later in the file.
        e.fork(10, 10, 11, 10, 200),
        e.comm(10, 11, "worker", false, 210),
        e.mmap2(10, 0x7f00_0000, 0x2000, 0, "/usr/lib/libm.so.6", 250),
        e.sample(
            10,
            11,
            400,
            0x7f00_0040,
            &[CONTEXT_USER, 0x7f00_0040, 0x40_0300],
        ),
        e.switch(10, 11, 450, true, true),
        finished_round(),
    ];
    let features = vec![
        (feature::HOSTNAME, string_feature("box")),
        (feature::ARCH, string_feature("x86_64")),
        (feature::BUILD_ID, build_id_entry("/opt/app", &[0xab; 20])),
    ];
    file(
        &[(
            attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL | FREQ, 1000),
            vec![7, 8],
        )],
        &records,
        &features,
    )
}

fn times(events: &[(u64, Event)]) -> Vec<u64> {
    events.iter().map(|(time, _)| *time).collect()
}

fn samples(events: &[(u64, Event)]) -> Vec<(u64, &Sample)> {
    events
        .iter()
        .filter_map(|(time, e)| {
            if let Event::Sample(s) = e {
                Some((*time, s))
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn a_file_is_read_in_time_order_with_its_attributes_and_features() {
    let (session, events) = read_all(&two_thread_file(), false).unwrap();
    assert_eq!(session.attrs().len(), 1);
    assert_eq!(session.attrs()[0].ids, vec![7, 8]);
    assert_eq!(session.attrs()[0].period_ns(), Some(1_000_000), "1000 Hz");
    assert_eq!(
        times(&events),
        vec![0, 100, 110, 200, 210, 250, 300, 400, 450, 500]
    );
    assert!(events.contains(&(
        450,
        Event::Switch {
            pid: 10,
            tid: 11,
            cpu: None,
            out: true,
            preempt: true
        }
    )));
    assert_eq!(session.stats.samples, 3);
    assert_eq!(session.info.hostname, "box");
    assert_eq!(session.info.arch, "x86_64");
    assert_eq!(
        session.info.build_ids.get("/opt/app"),
        Some(&vec![0xab; 20])
    );

    let samples = samples(&events);
    // The caller's frame is its return address minus one: inside the call.
    assert_eq!(
        samples[0].1.frames,
        vec![
            Frame {
                address: 0x40_0010,
                kernel: false
            },
            Frame {
                address: 0x40_01ff,
                kernel: false
            }
        ]
    );
    assert_eq!((samples[1].1.pid, samples[1].1.tid), (10, 11));
    // The markers are gone and each frame knows its side. The first
    // address on the user side is where the program entered the kernel: a
    // pc, like the sampled one, and left as it is.
    assert_eq!(
        samples[2].1.frames,
        vec![
            Frame {
                address: 0xffff_ffff_8000_0123,
                kernel: true
            },
            Frame {
                address: 0x40_0020,
                kernel: false
            },
            Frame {
                address: 0x40_01ff,
                kernel: false
            },
        ]
    );
    assert_eq!(Info::read(&two_thread_file()).unwrap().hostname, "box");
}

#[test]
fn frames_are_labelled_by_module_and_offset_and_threads_by_their_comm() {
    let (_, events) = read_all(&two_thread_file(), false).unwrap();
    let mut processes = Processes::default();
    let mut labels = Vec::new();
    for (_, event) in &events {
        processes.apply(event);
        if let Event::Sample(s) = event {
            labels.push(
                s.frames
                    .iter()
                    .map(|f| processes.label(s.pid, *f))
                    .collect::<Vec<_>>(),
            );
        }
    }
    assert_eq!(labels[0], ["app+0x1010", "app+0x11ff"]);
    assert_eq!(labels[1], ["libm.so.6+0x40", "app+0x12ff"]);
    assert_eq!(labels[2], ["[kernel]+0x123", "app+0x1020", "app+0x11ff"]);
    assert_eq!(processes.thread_name(10), Some("app"));
    assert_eq!(processes.thread_name(11), Some("worker"));
    assert_eq!(processes.process_name(10), Some("app"));
    assert_eq!(processes.pid_of(11), Some(10));
    assert_eq!(
        processes.label(
            10,
            Frame {
                address: 0x1234,
                kernel: false
            }
        ),
        "0x1234",
        "in no mapping"
    );
}

/// Cut into chunks of any size, a file gives the events one read gives.
#[test]
fn a_file_read_in_pieces_gives_the_same_events() {
    let bytes = two_thread_file();
    let (_, whole) = read_all(&bytes, false).unwrap();
    for chunk in [1, 3, 7, 64, 1000] {
        let mut session = Session::new(false);
        let mut events = Vec::new();
        let mut held: Vec<u8> = Vec::new();
        let mut pieces = bytes.chunks(chunk);
        let mut ended = false;
        while !session.is_done() {
            match session.step(&held, ended).unwrap() {
                Some(used) => {
                    held.drain(..used);
                }
                None => match pieces.next() {
                    Some(piece) => held.extend_from_slice(piece),
                    None => ended = true,
                },
            }
            while let Some(event) = session.next_event() {
                events.push(event);
            }
        }
        assert_eq!(events, whole, "in chunks of {chunk}");
    }
}

/// `perf` writes the sizes when it exits. Killed, it leaves a data size of
/// zero and no features; cut short, the file ends inside a record.
#[test]
fn a_recording_that_was_killed_is_read_as_far_as_it_goes() {
    let whole = two_thread_file();
    let (_, all) = read_all(&whole, false).unwrap();

    // No size: the records run to the end of the file, and what follows
    // them here (the features, which are not records) ends the reading
    // without taking back what was read.
    let mut no_size = whole.clone();
    no_size[48..56].fill(0);
    let (session, events) = read_all(&no_size, false).unwrap();
    assert_eq!(events, all);
    assert!(
        session.info.hostname.is_empty(),
        "nothing says where the features are"
    );

    let data_at = u64::from_le_bytes(whole[40..48].try_into().unwrap()) as usize;
    let cut = &whole[..data_at + 400];
    let (_, events) = read_all(cut, false).unwrap();
    assert!(
        !events.is_empty() && events.len() < all.len(),
        "{} of {}",
        events.len(),
        all.len()
    );
    assert_eq!(times(&events), {
        let mut t = times(&events);
        t.sort();
        t
    });
}

#[test]
fn what_is_not_a_perf_data_is_refused_with_its_reason() {
    assert!(is_perf_data(&two_thread_file()));
    assert!(!is_perf_data(b"PERFILE"));
    assert!(!is_perf_data(b"{\"traceEvents\":[]}"));
    let error = |bytes: &[u8]| read_all(bytes, false).err().unwrap();
    assert!(error(b"not a perf.data at all").contains("PERFILE2"));
    assert!(error(b"2ELIFREP\0\0\0\0\0\0\0\x68 and more").contains("big-endian"));
    assert!(error(&two_thread_file()[..60]).contains("header"));
    // A record whose size is under its own header would never advance.
    let mut bad = two_thread_file();
    let data_at = u64::from_le_bytes(bad[40..48].try_into().unwrap()) as usize;
    bad[data_at + 6..data_at + 8].fill(0);
    assert!(error(&bad).contains("record of 0 bytes"));
}

/// Events ask different things of their samples; the id at the front of
/// each record says which event's layout it has. Here the first takes call
/// chains and the second does not.
#[test]
fn each_sample_is_read_with_the_layout_of_its_own_event() {
    let plain = IDENTIFIER | IP | TID | TIME;
    let a = Event1 {
        sample_type: CALLCHAIN_SAMPLE,
        id: 1,
    };
    let b = Event1 {
        sample_type: plain,
        id: 2,
    };
    let records = vec![
        a.sample(5, 5, 10, 0x1000, &[CONTEXT_USER, 0x1000, 0x2000]),
        b.sample(5, 6, 20, 0x3000, &[]),
        b.comm(5, 6, "second", false, 5),
    ];
    let bytes = file(
        &[
            (attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![1]),
            (attr(plain, SAMPLE_ID_ALL, 0), vec![2]),
        ],
        &records,
        &[],
    );
    let (_, events) = read_all(&bytes, false).unwrap();
    assert_eq!(times(&events), vec![5, 10, 20]);
    let samples = samples(&events);
    assert_eq!((samples[0].1.attr, samples[0].1.frames.len()), (0, 2));
    assert_eq!((samples[1].1.attr, samples[1].1.tid), (1, 6));
    assert_eq!(
        samples[1].1.frames,
        vec![Frame {
            address: 0x3000,
            kernel: false
        }],
        "no chain: the sampled pc"
    );
}

/// `--call-graph dwarf`: after the chain come the registers and a copy of
/// the stack, and before it whatever else was asked for. Every field in
/// front of the ones read has to be stepped over exactly.
#[test]
fn registers_and_stack_follow_fields_that_are_only_stepped_over() {
    let sample_type =
        IP | TID | TIME | ADDR | READ | CALLCHAIN | RAW | BRANCH_STACK | REGS_USER | STACK_USER;
    let mut a = attr(sample_type, SAMPLE_ID_ALL, 0);
    // read_format: GROUP | ID, branch_sample_type: HW_INDEX, and three
    // registers in the mask.
    a[32..40].copy_from_slice(&(8u64 | 4).to_le_bytes());
    a[72..80].copy_from_slice(&(1u64 << 17).to_le_bytes());
    a[80..88].copy_from_slice(&0b1011u64.to_le_bytes());
    let stack: Vec<u8> = (0..32).collect();
    let body = Bytes::default()
        .u64(0x1000) // ip
        .u32(9)
        .u32(9)
        .u64(77) // time
        .u64(0xdead) // addr
        .u64(2) // read: two values, each with its id
        .u64(1)
        .u64(101)
        .u64(2)
        .u64(102)
        .u64(1) // chain
        .u64(0x1000)
        .u32(12) // raw, with its padding
        .raw(&[9; 12])
        .u64(1) // one branch, after the hw index
        .u64(u64::MAX)
        .raw(&[0; 24])
        .u64(2) // regs: abi 64
        .u64(0x11)
        .u64(0x22)
        .u64(0x33)
        .u64(32) // stack: 32 bytes asked, 24 copied
        .raw(&stack)
        .u64(24);
    let bytes = file(
        &[(a, vec![1])],
        &[record(record::SAMPLE, MISC_USER, &body.0)],
        &[],
    );

    let (_, events) = read_all(&bytes, true).unwrap();
    let samples_kept = samples(&events);
    let (time, sample) = samples_kept[0];
    assert_eq!((time, sample.pid), (77, 9));
    assert_eq!(
        sample.frames,
        vec![Frame {
            address: 0x1000,
            kernel: false
        }]
    );
    let user = sample.user.as_ref().expect("the registers and the stack");
    assert_eq!((user.abi, &user.regs[..]), (2, &[0x11, 0x22, 0x33][..]));
    assert_eq!(user.stack, &stack[..24], "only what was copied");

    let (_, events) = read_all(&bytes, false).unwrap();
    assert!(
        samples(&events)[0].1.user.is_none(),
        "not kept unless asked for"
    );
}

#[test]
fn a_new_mapping_replaces_what_it_covers_and_a_fork_inherits() {
    let map = |pid: u32, start: u64, len: u64, pgoff: u64, path: &str| {
        Event::Mmap(Mmap {
            pid,
            start,
            len,
            pgoff,
            path: path.into(),
            build_id: Vec::new(),
            kernel: false,
        })
    };
    let user = |address: u64| Frame {
        address,
        kernel: false,
    };
    let mut p = Processes::default();
    p.apply(&map(1, 0x1000, 0x3000, 0, "/lib/old.so"));
    p.apply(&map(1, 0x2000, 0x1000, 0x5000, "/lib/new.so"));
    // The old mapping is still there on both sides of the new one, and
    // its right half is still at the right place in its file.
    assert_eq!(p.label(1, user(0x1800)), "old.so+0x800");
    assert_eq!(p.label(1, user(0x2800)), "new.so+0x5800");
    assert_eq!(p.label(1, user(0x3800)), "old.so+0x2800");
    assert_eq!(p.mappings(1).len(), 3);

    p.apply(&Event::Comm {
        pid: 1,
        tid: 1,
        name: "parent".into(),
        exec: false,
    });
    p.apply(&Event::Fork {
        pid: 2,
        ppid: 1,
        tid: 2,
        ptid: 1,
    });
    assert_eq!(
        p.label(2, user(0x2800)),
        "new.so+0x5800",
        "a child starts with its parent's mappings"
    );
    assert_eq!(p.thread_name(2), Some("parent"), "and its name");
    // An exec is a new program: its name, and none of the old mappings.
    p.apply(&Event::Comm {
        pid: 2,
        tid: 2,
        name: "child".into(),
        exec: true,
    });
    assert_eq!(p.label(2, user(0x2800)), "0x2800");
    assert_eq!(p.process_name(2), Some("child"));
    assert_eq!(
        p.label(1, user(0x2800)),
        "new.so+0x5800",
        "the parent keeps its own"
    );

    // Memory with no file behind it is named by address; the vdso and a
    // kernel module by name.
    p.apply(&map(1, 0x9000, 0x1000, 0, "//anon"));
    p.apply(&map(1, 0xa000, 0x1000, 0, "[vdso]"));
    assert_eq!(p.label(1, user(0x9010)), "0x9010");
    assert_eq!(p.label(1, user(0xa010)), "[vdso]+0x10");
    p.apply(&Event::Mmap(Mmap {
        pid: u32::MAX,
        start: 0xffff_0000,
        len: 0x1000,
        pgoff: 0,
        path: "[nvidia]".into(),
        build_id: Vec::new(),
        kernel: true,
    }));
    assert_eq!(
        p.label(
            1,
            Frame {
                address: 0xffff_0020,
                kernel: true
            }
        ),
        "[nvidia]+0x20"
    );
    // A module `perf` found the file of is recorded by its path: still a
    // module, by the name the kernel knows it under.
    let path = "/lib/modules/6.1.0/kernel/drivers/nvidia-uvm.ko.zst";
    p.apply(&Event::Mmap(Mmap {
        pid: u32::MAX,
        start: 0xfffe_0000,
        len: 0x1000,
        pgoff: 0,
        path: path.into(),
        build_id: Vec::new(),
        kernel: true,
    }));
    assert_eq!(
        p.label(
            1,
            Frame {
                address: 0xfffe_0030,
                kernel: true
            }
        ),
        "[nvidia_uvm]+0x30"
    );
    assert_eq!(
        p.kernel_mappings()[0].kernel_module().as_deref(),
        Some("nvidia_uvm")
    );
    assert!(!p.kernel_mappings()[0].has_file() && p.mappings(1)[0].has_file());
}

/// `perf record -o -`: a 16-byte header, then records only. The
/// attributes and the features are records too, and two kinds of record
/// are followed by a payload their size does not count.
#[test]
fn a_piped_recording_carries_its_attributes_and_features_as_records() {
    let e = Event1 {
        sample_type: CALLCHAIN_SAMPLE,
        id: 7,
    };
    let attr_record = record(
        record::HEADER_ATTR,
        0,
        &Bytes::default()
            .raw(&attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL | FREQ, 100))
            .u64(7)
            .0,
    );
    let auxtrace = record(
        record::AUXTRACE,
        0,
        &Bytes::default().u64(13).raw(&[0; 32]).0,
    );
    let tracing = record(record::HEADER_TRACING_DATA, 0, &Bytes::default().u32(5).0);
    let host = record(
        record::HEADER_FEATURE,
        0,
        &Bytes::default()
            .u64(u64::from(feature::HOSTNAME))
            .raw(&string_feature("pipe-host"))
            .0,
    );
    let build_id = build_id_entry("/opt/app", &[0xcd; 20]);
    let build_id = [&record(record::HEADER_BUILD_ID, MISC_USER, &build_id[8..])[..]].concat();
    let records: Vec<u8> = [
        attr_record,
        host,
        build_id,
        e.comm(3, 3, "piped", true, 1),
        auxtrace,
        vec![0xee; 13],
        e.sample(3, 3, 5, 0x10, &[CONTEXT_USER, 0x10]),
        tracing,
        // Five bytes, padded to eight.
        vec![0xee; 8],
        e.sample(3, 3, 6, 0x20, &[CONTEXT_USER, 0x20]),
    ]
    .concat();
    let bytes = Bytes::default().raw(&MAGIC).u64(16).raw(&records).0;

    let (session, events) = read_all(&bytes, false).unwrap();
    assert_eq!(session.attrs().len(), 1);
    assert_eq!(session.attrs()[0].period_ns(), Some(10_000_000));
    assert_eq!(session.info.hostname, "pipe-host");
    assert_eq!(
        session.info.build_ids.get("/opt/app"),
        Some(&vec![0xcd; 20])
    );
    assert_eq!(times(&events), vec![1, 5, 6]);
}

/// `perf record -z`: the ring data is one zstd stream cut into records
/// wherever a write fell, in the middle of a block or of a record. What
/// `perf` writes itself (the round marks) is not compressed.
#[cfg(feature = "zstd")]
#[test]
fn a_compressed_recording_is_one_stream_cut_into_records() {
    let e = Event1 {
        sample_type: CALLCHAIN_SAMPLE,
        id: 7,
    };
    let mut plain = vec![
        e.comm(4, 4, "zipped", true, 1),
        e.mmap2(4, 0x1000, 0x1000, 0, "/opt/z", 2),
    ];
    for i in 0..200u64 {
        plain.push(e.sample(
            4,
            4,
            10 + i,
            0x1000 + i,
            &[CONTEXT_USER, 0x1000 + i, 0x1800],
        ));
    }
    let expected: Vec<Vec<u8>> = plain.clone();
    let stream = zstd_raw(&plain.concat(), 700);
    for cut in [1usize, 5, 333, 4096] {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for (i, piece) in stream.chunks(cut).enumerate() {
            // Both record kinds: the newer one says how long its data is
            // and pads it.
            if i % 2 == 0 {
                records.push(record(record::COMPRESSED, 0, piece));
            } else {
                let padded = Bytes::default()
                    .u64(piece.len() as u64)
                    .raw(piece)
                    .raw(&[0; 8][..(8 - piece.len() % 8) % 8])
                    .0;
                records.push(record(record::COMPRESSED2, 0, &padded));
            }
        }
        records.push(finished_round());
        let bytes = file(
            &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
            &records,
            &[],
        );
        let (session, events) = read_all(&bytes, false).unwrap();
        assert_eq!(session.stats.samples, 200, "cut every {cut}");
        assert_eq!(events.len(), expected.len(), "cut every {cut}");
        assert_eq!(times(&events), (1..=2).chain(10..210).collect::<Vec<u64>>());
    }
}

/// Four bytes of zstd are 128 KiB of one byte: a record full of those is
/// refused rather than decoded into gigabytes.
#[cfg(feature = "zstd")]
#[test]
fn a_compressed_record_that_decodes_without_bound_is_refused() {
    let mut stream = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x58];
    for _ in 0..12_000 {
        // An RLE block: type 1, 128 KiB of the byte that follows.
        let header = (131_072u32 << 3) | (1 << 1);
        stream.extend(&header.to_le_bytes()[..3]);
        stream.push(0);
    }
    let bytes = file(
        &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
        &[record(record::COMPRESSED, 0, &stream)],
        &[],
    );
    let error = read_all(&bytes, false).err().unwrap();
    assert!(error.contains("256 MiB"), "{error}");
}

/// A build without the decoder says what it cannot read, and how to make
/// a recording it can.
#[cfg(not(feature = "zstd"))]
#[test]
fn a_compressed_recording_is_refused_by_a_build_without_zstd() {
    let stream = zstd_raw(b"anything", 700);
    let bytes = file(
        &[(attr(CALLCHAIN_SAMPLE, SAMPLE_ID_ALL, 0), vec![7])],
        &[record(record::COMPRESSED, 0, &stream)],
        &[],
    );
    let error = read_all(&bytes, false).err().unwrap();
    assert!(error.contains("perf record -z"), "{error}");
}
