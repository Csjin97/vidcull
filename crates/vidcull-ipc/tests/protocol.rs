use vidcull_core::{decode, encode};
use vidcull_ipc::protocol::{
    Action, ActionResult, ClipOverlap, ClusterMemberDetail, ClusterStats, ClusterSummary,
    CrossGroupConflict, DaemonSettings, DeleteRequest, DeleteResult, FailedTask, FileDetail,
    GroupRole, GroupStats, GroupSummary, IpcError, IpcErrorKind, LogLevel, LogRecord,
    ProgressSnapshot, Request, Response, TrustLevel, UndoResult,
};

fn sample_detail(file_id: i64, is_best: bool) -> FileDetail {
    FileDetail {
        file_id,
        path: format!("/library/{file_id}.mp4"),
        size_bytes: 1_000 + file_id,
        width: Some(1920),
        height: Some(1080),
        duration_ms: Some(60_000),
        bitrate_bps: Some(8_000_000),
        codec: Some("h264".to_owned()),
        container: Some("mp4".to_owned()),
        is_best,
        thumbnail: None,
    }
}

fn round_trip_request(request: &Request) {
    let bytes = encode(request).expect("encode request");
    let decoded: Request = decode(&bytes).expect("decode request");
    assert_eq!(&decoded, request);
}

fn round_trip_response(response: &Response) {
    let bytes = encode(response).expect("encode response");
    let decoded: Response = decode(&bytes).expect("decode response");
    assert_eq!(&decoded, response);
}

#[test]
fn every_request_variant_round_trips() {
    round_trip_request(&Request::Ping);
    round_trip_request(&Request::Progress);
    round_trip_request(&Request::ListGroups {
        trust: Some(TrustLevel::VeryLikely),
        limit: 50,
        offset: 100,
    });
    round_trip_request(&Request::ListGroups {
        trust: None,
        limit: 0,
        offset: 0,
    });
    round_trip_request(&Request::Action(Action::Rescan {
        path: "/library/movies".to_owned(),
    }));
    round_trip_request(&Request::Action(Action::Shutdown));
    round_trip_request(&Request::StreamLogs { max_records: 256 });
    round_trip_request(&Request::GroupDetail { group_id: 7 });
    round_trip_request(&Request::GroupStats {
        trust: Some(TrustLevel::Exact),
    });
    round_trip_request(&Request::GroupStats { trust: None });
    round_trip_request(&Request::Action(Action::MoveToTrash(DeleteRequest {
        group_id: 7,
        file_ids: vec![11, 12],
        confirm_best: false,
    })));
    round_trip_request(&Request::Action(Action::DeletePermanent(DeleteRequest {
        group_id: 7,
        file_ids: vec![13],
        confirm_best: true,
    })));
    round_trip_request(&Request::PartialOverlaps { group_id: 9 });
    round_trip_request(&Request::GetSettings);
    round_trip_request(&Request::Action(Action::SetSettings(DaemonSettings {
        scan_folders: vec!["C:/videos".to_owned()],
        background_enabled: true,
        auto_index: false,
        exclude_rules: vec!["node_modules".to_owned()],
        run_on_boot: true,
        cpu_throttle: vidcull_ipc::CpuThrottle::Balanced,
        best_copy_mode: vidcull_ipc::BestCopyMode::SpaceSaving,
        idle_worker_count: Some(6),
        cpu_cores: 16,
        partial_clips_enabled: true,
        indexing_enabled: false,
    })));
    round_trip_request(&Request::ClusterSummaries {
        trust: Some(TrustLevel::VeryLikely),
        limit: 30,
        offset: 60,
    });
    round_trip_request(&Request::ClusterSummaries {
        trust: None,
        limit: 0,
        offset: 0,
    });
    round_trip_request(&Request::ClusterDetail { cluster_id: 12 });
    round_trip_request(&Request::ClusterStats {
        trust: Some(TrustLevel::Possible),
    });
    round_trip_request(&Request::ClusterStats { trust: None });
    round_trip_request(&Request::FailedTasks { limit: 50 });
    round_trip_request(&Request::FailedTasks { limit: 0 });
    round_trip_request(&Request::CrossGroupConflicts { group_id: 7 });
    round_trip_request(&Request::Thumbnail { file_id: 42 });
    round_trip_request(&Request::Action(Action::UndoLastDelete));
    round_trip_request(&Request::Action(Action::ForceRescan {
        path: "/library/movies".to_owned(),
    }));
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_response_variant_round_trips() {
    round_trip_response(&Response::Pong {
        protocol_version: 1,
    });
    round_trip_response(&Response::Progress(ProgressSnapshot {
        pending: 12,
        running: 1,
        done: 9_000,
        failed: 3,
        cpu_usage_permille: 125,
        rss_bytes: 104_857_600,
        throughput_bytes_per_sec: 1_500_000,
        pending_bytes: 6_400_000_000,
        current_files: vec![
            "C:/videos/movie.mkv".to_owned(),
            "C:/videos/대용량 영상.mp4".to_owned(),
        ],
        dead_workers: 0,
        panic_count: 0,
        partial_pending: 7,
        partial_running: 1,
        partial_done: 5,
        partial_skipped: std::collections::BTreeMap::from([
            ("unsupported-codec".to_owned(), 2),
            ("duration-cap".to_owned(), 1),
        ]),
        partial_failed: 4,
        folder_scanning: false,
        scan_discovered: 0,
        groups_revision: 42,
    }));
    round_trip_response(&Response::Groups(vec![
        GroupSummary {
            group_id: 1,
            trust: TrustLevel::Exact,
            best_file_id: Some(42),
            member_count: 3,
            intro_outro: false,
        },
        GroupSummary {
            group_id: 2,
            trust: TrustLevel::Possible,
            best_file_id: None,
            member_count: 2,
            intro_outro: true,
        },
    ]));
    round_trip_response(&Response::Action(ActionResult {
        accepted: true,
        detail: "enqueued task 7".to_owned(),
    }));
    round_trip_response(&Response::Log(LogRecord {
        timestamp_ms: 1_700_000_000_000,
        level: LogLevel::Warn,
        target: "vidcull_daemon::watcher".to_owned(),
        message: "watch error; continuing".to_owned(),
    }));
    round_trip_response(&Response::StreamEnd);
    round_trip_response(&Response::Error(IpcError::new(
        IpcErrorKind::NotFound,
        "no such group",
    )));
    round_trip_response(&Response::GroupDetail(vec![
        FileDetail {
            file_id: 42,
            path: "/library/movies/a.mp4".to_owned(),
            size_bytes: 1_234_567_890,
            width: Some(3840),
            height: Some(2160),
            duration_ms: Some(5_400_000),
            bitrate_bps: Some(18_000_000),
            codec: Some("hevc".to_owned()),
            container: Some("mp4".to_owned()),
            is_best: true,
            thumbnail: Some("data:image/jpeg;base64,/9j/AAAA".to_owned()),
        },
        FileDetail {
            file_id: 43,
            path: "/library/movies/a_reencode.mkv".to_owned(),
            size_bytes: 456_789,
            width: None,
            height: None,
            duration_ms: None,
            bitrate_bps: None,
            codec: None,
            container: None,
            is_best: false,
            thumbnail: None,
        },
    ]));
    round_trip_response(&Response::GroupDetail(Vec::new()));
    round_trip_response(&Response::GroupStats(GroupStats {
        group_count: 1_280,
        reclaimable_bytes: 9_999_999_999,
    }));
    round_trip_response(&Response::Delete(DeleteResult {
        ok: true,
        removed_file_ids: vec![11, 12],
        reclaimed_bytes: 1_456_789,
        detail: "2개 파일을 휴지통으로 이동했습니다.".to_owned(),
        reject_code: None,
    }));
    round_trip_response(&Response::Delete(DeleteResult {
        ok: false,
        removed_file_ids: Vec::new(),
        reclaimed_bytes: 0,
        detail: String::new(),
        reject_code: Some("DELETE_ALL".to_owned()),
    }));
    round_trip_response(&Response::PartialOverlaps(vec![ClipOverlap {
        clip_file_id: 43,
        source_file_id: 42,
        matched_scenes: 9,
        clip_scenes: 12,
        start_ms: 30_000,
        end_ms: 90_000,
        clip_start_ms: 0,
        clip_end_ms: 60_000,
        intro_outro: false,
    }]));
    round_trip_response(&Response::PartialOverlaps(Vec::new()));
    round_trip_response(&Response::Settings(DaemonSettings::default()));
    round_trip_response(&Response::Settings(DaemonSettings {
        scan_folders: vec!["C:/a".to_owned(), "D:/b".to_owned()],
        background_enabled: false,
        auto_index: true,
        exclude_rules: vec![".trash".to_owned()],
        run_on_boot: true,
        cpu_throttle: vidcull_ipc::CpuThrottle::Eco,
        best_copy_mode: vidcull_ipc::BestCopyMode::Archival,
        idle_worker_count: None,
        cpu_cores: 32,
        partial_clips_enabled: false,
        indexing_enabled: true,
    }));
    round_trip_response(&Response::ClusterSummaries(vec![
        ClusterSummary {
            cluster_id: 1,
            representative_trust: TrustLevel::Exact,
            best_file_id: Some(1),
            member_count: 3,
            member_trust_levels: vec![TrustLevel::Exact, TrustLevel::VeryLikely],
            intro_outro: false,
            members: Vec::new(),
        },
        ClusterSummary {
            cluster_id: 9,
            representative_trust: TrustLevel::Possible,
            best_file_id: None,
            member_count: 2,
            member_trust_levels: vec![TrustLevel::Possible],
            intro_outro: true,
            members: Vec::new(),
        },
    ]));
    round_trip_response(&Response::ClusterSummaries(Vec::new()));
    round_trip_response(&Response::ClusterDetail(vec![
        ClusterMemberDetail {
            file: sample_detail(1, true),
            trust: TrustLevel::Exact,
            group_id: 100,
        },
        ClusterMemberDetail {
            file: sample_detail(2, false),
            trust: TrustLevel::VeryLikely,
            group_id: 101,
        },
    ]));
    round_trip_response(&Response::ClusterDetail(Vec::new()));
    round_trip_response(&Response::ClusterStats(ClusterStats {
        cluster_count: 42,
        reclaimable_bytes: 5_000_000,
    }));
    round_trip_response(&Response::FailedTasks(vec![
        FailedTask {
            task_id: 7,
            path: "/library/broken.mkv".to_owned(),
            reason: "decode error: invalid stream".to_owned(),
            attempts: 3,
        },
        FailedTask {
            task_id: 8,
            path: String::new(),
            reason: "task failed".to_owned(),
            attempts: 1,
        },
    ]));
    round_trip_response(&Response::FailedTasks(Vec::new()));
    round_trip_response(&Response::CrossGroupConflicts(vec![CrossGroupConflict {
        file_id: 42,
        path: "/library/movies/keep-or-clip.mp4".to_owned(),
        memberships: vec![
            GroupRole {
                group_id: 1,
                trust: TrustLevel::Exact,
                is_best: true,
            },
            GroupRole {
                group_id: 9,
                trust: TrustLevel::Possible,
                is_best: false,
            },
        ],
    }]));
    round_trip_response(&Response::CrossGroupConflicts(Vec::new()));
    round_trip_response(&Response::Thumbnail(Some(
        "data:image/jpeg;base64,/9j/AAAA".to_owned(),
    )));
    round_trip_response(&Response::Thumbnail(None));
    round_trip_response(&Response::Undo(UndoResult {
        ok: true,
        group_id: Some(7),
        restored_file_ids: vec![11, 12],
        missing_paths: vec!["/library/movies/still-in-trash.mp4".to_owned()],
        detail: "2개 파일을 복원했습니다.".to_owned(),
    }));
    round_trip_response(&Response::Undo(UndoResult {
        ok: false,
        group_id: None,
        restored_file_ids: Vec::new(),
        missing_paths: Vec::new(),
        detail: "되돌릴 삭제 내역이 없습니다.".to_owned(),
    }));
}

#[test]
fn empty_groups_page_round_trips() {
    round_trip_response(&Response::Groups(Vec::new()));
}

#[test]
fn file_detail_thumbnail_round_trips_both_states() {
    for thumbnail in [Some("data:image/jpeg;base64,/9j/4AAQ".to_owned()), None] {
        let detail = FileDetail {
            file_id: 7,
            path: "/library/clip.mp4".to_owned(),
            size_bytes: 1024,
            width: Some(1920),
            height: Some(1080),
            duration_ms: Some(60_000),
            bitrate_bps: Some(8_000_000),
            codec: Some("h264".to_owned()),
            container: Some("mp4".to_owned()),
            is_best: false,
            thumbnail: thumbnail.clone(),
        };
        let bytes = encode(&detail).expect("encode detail");
        let decoded: FileDetail = decode(&bytes).expect("decode detail");
        assert_eq!(decoded.thumbnail, thumbnail);
        assert_eq!(decoded, detail);
    }
}

#[test]
fn trust_level_discriminants_are_stable() {
    assert_eq!(encode(&TrustLevel::Exact).expect("encode"), vec![0]);
    assert_eq!(encode(&TrustLevel::VeryLikely).expect("encode"), vec![1]);
    assert_eq!(encode(&TrustLevel::Possible).expect("encode"), vec![2]);
}

#[test]
fn v2_variants_are_appended_after_v1() {
    assert_eq!(
        encode(&Request::GroupDetail { group_id: 1 }).expect("encode")[0],
        5
    );
    assert_eq!(
        encode(&Request::GroupStats { trust: None }).expect("encode")[0],
        6
    );
    assert_eq!(
        encode(&Response::GroupDetail(Vec::new())).expect("encode")[0],
        7
    );
    assert_eq!(
        encode(&Response::GroupStats(GroupStats::default())).expect("encode")[0],
        8
    );
}

#[test]
fn v3_variants_are_appended_after_v2() {
    assert_eq!(
        encode(&Request::PartialOverlaps { group_id: 1 }).expect("encode")[0],
        7
    );
    assert_eq!(
        encode(&Response::Delete(DeleteResult {
            ok: true,
            removed_file_ids: Vec::new(),
            reclaimed_bytes: 0,
            detail: String::new(),
            reject_code: None,
        }))
        .expect("encode")[0],
        9
    );
    assert_eq!(
        encode(&Response::PartialOverlaps(Vec::new())).expect("encode")[0],
        10
    );

    let move_to_trash = encode(&Request::Action(Action::MoveToTrash(DeleteRequest {
        group_id: 1,
        file_ids: Vec::new(),
        confirm_best: false,
    })))
    .expect("encode");
    assert_eq!(move_to_trash[0], 3, "Request::Action tag");
    assert_eq!(move_to_trash[1], 2, "Action::MoveToTrash tag");
    let delete_permanent = encode(&Request::Action(Action::DeletePermanent(DeleteRequest {
        group_id: 1,
        file_ids: Vec::new(),
        confirm_best: false,
    })))
    .expect("encode");
    assert_eq!(delete_permanent[1], 3, "Action::DeletePermanent tag");
}

#[test]
fn v4_variants_are_appended_after_v3() {
    assert_eq!(encode(&Request::GetSettings).expect("encode")[0], 8);
    assert_eq!(
        encode(&Response::Settings(DaemonSettings::default())).expect("encode")[0],
        11
    );
    let set_settings = encode(&Request::Action(Action::SetSettings(
        DaemonSettings::default(),
    )))
    .expect("encode");
    assert_eq!(set_settings[0], 3, "Request::Action tag");
    assert_eq!(set_settings[1], 4, "Action::SetSettings tag");
}

#[test]
fn v6_variants_are_appended_after_v4() {
    assert_eq!(
        encode(&Request::ClusterSummaries {
            trust: None,
            limit: 0,
            offset: 0,
        })
        .expect("encode")[0],
        9
    );
    assert_eq!(
        encode(&Request::ClusterDetail { cluster_id: 1 }).expect("encode")[0],
        10
    );
    assert_eq!(
        encode(&Request::ClusterStats { trust: None }).expect("encode")[0],
        11
    );
    assert_eq!(
        encode(&Response::ClusterSummaries(Vec::new())).expect("encode")[0],
        12
    );
    assert_eq!(
        encode(&Response::ClusterDetail(Vec::new())).expect("encode")[0],
        13
    );
    assert_eq!(
        encode(&Response::ClusterStats(ClusterStats::default())).expect("encode")[0],
        14
    );
}

#[test]
fn v7_variants_are_appended_after_v6() {
    assert_eq!(
        encode(&Request::FailedTasks { limit: 0 }).expect("encode")[0],
        12
    );
    assert_eq!(
        encode(&Response::FailedTasks(Vec::new())).expect("encode")[0],
        15
    );
}

#[test]
fn v9_variants_are_appended_after_v7() {
    assert_eq!(
        encode(&Request::CrossGroupConflicts { group_id: 1 }).expect("encode")[0],
        13
    );
    assert_eq!(
        encode(&Response::CrossGroupConflicts(Vec::new())).expect("encode")[0],
        16
    );
}

#[test]
fn v10_variants_are_appended_after_v9() {
    let undo = encode(&Request::Action(Action::UndoLastDelete)).expect("encode");
    assert_eq!(undo[0], 3, "Request::Action tag");
    assert_eq!(undo[1], 5, "Action::UndoLastDelete tag");
    assert_eq!(
        encode(&Response::Undo(UndoResult {
            ok: false,
            group_id: None,
            restored_file_ids: Vec::new(),
            missing_paths: Vec::new(),
            detail: String::new(),
        }))
        .expect("encode")[0],
        17
    );
}

#[test]
fn v16_force_rescan_is_appended_after_undo() {
    let force = encode(&Request::Action(Action::ForceRescan {
        path: "/lib".to_owned(),
    }))
    .expect("encode");
    assert_eq!(force[0], 3, "Request::Action tag");
    assert_eq!(force[1], 6, "Action::ForceRescan tag");
    assert_eq!(
        encode(&Request::Action(Action::Rescan {
            path: "/lib".to_owned()
        }))
        .expect("encode")[1],
        0,
        "Action::Rescan tag unchanged",
    );
    assert_eq!(
        encode(&Request::Action(Action::UndoLastDelete)).expect("encode")[1],
        5,
        "Action::UndoLastDelete tag unchanged",
    );
}

#[test]
fn v21_log_level_and_export_are_appended_after_force_rescan() {
    use vidcull_ipc::protocol::LogLevel;
    let set_level = encode(&Request::Action(Action::SetLogLevel(LogLevel::Debug))).expect("encode");
    assert_eq!(set_level[0], 3, "Request::Action tag");
    assert_eq!(set_level[1], 7, "Action::SetLogLevel tag");

    let export = encode(&Request::Action(Action::ExportDiagnostics {
        dest: "/tmp/bundle".to_owned(),
    }))
    .expect("encode");
    assert_eq!(export[1], 8, "Action::ExportDiagnostics tag");

    assert_eq!(
        encode(&Request::Action(Action::ForceRescan {
            path: "/lib".to_owned()
        }))
        .expect("encode")[1],
        6,
        "Action::ForceRescan tag unchanged",
    );
}

#[test]
fn ping_encodes_to_a_single_byte() {
    assert_eq!(encode(&Request::Ping).expect("encode"), vec![0]);
}

#[test]
fn decoding_garbage_is_an_error_not_a_panic() {
    let err = decode::<Request>(&[0xFF, 0xFF, 0xFF]).expect_err("garbage must fail");
    assert!(
        matches!(err, vidcull_core::Error::Serialization(_)),
        "expected Serialization error, got {err:?}"
    );
}
