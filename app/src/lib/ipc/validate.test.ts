
import { describe, expect, it } from "vitest";
import {
  IpcValidationError,
  validateProgressSnapshot,
  validateGroupStats,
  validateClusterStats,
  validateGroupSummaryArray,
  validateFileDetailArray,
  validateClusterMemberDetailArray,
  validateDeleteResult,
  validateUndoResult,
  validateClipOverlapArray,
  validateFailedTaskArray,
  validateCrossGroupConflictArray,
  validateWireSettings,
} from "./validate";


describe("IpcValidationError", () => {
  it("names the command and field in the message", () => {
    const err = new IpcValidationError("my_cmd", "some_field", undefined);
    expect(err.name).toBe("IpcValidationError");
    expect(err.message).toContain("my_cmd");
    expect(err.message).toContain("some_field");
    expect(err.command).toBe("my_cmd");
    expect(err.field).toBe("some_field");
  });
});


describe("validateProgressSnapshot", () => {
  it("accepts a valid payload", () => {
    expect(
      validateProgressSnapshot("daemon_progress", {
        pending: 1,
        running: 2,
        done: 3,
        failed: 0,
      }),
    ).toEqual({ pending: 1, running: 2, done: 3, failed: 0 });
  });

  it("ignores unknown extra fields (forward-compat)", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        future_field: "ignored",
      }),
    ).not.toThrow();
  });

  it("throws IpcValidationError naming the missing field", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", { pending: 1, running: 0, done: 0 }),
    ).toThrow(IpcValidationError);
    try {
      validateProgressSnapshot("daemon_progress", { pending: 1, running: 0, done: 0 });
    } catch (e) {
      expect((e as IpcValidationError).field).toBe("failed");
    }
  });

  it("throws when the root is not an object", () => {
    expect(() => validateProgressSnapshot("daemon_progress", null)).toThrow(
      IpcValidationError,
    );
    expect(() => validateProgressSnapshot("daemon_progress", [1, 2])).toThrow(
      IpcValidationError,
    );
  });

  it("passes the system-metrics fields through when present", () => {
    expect(
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 1,
        done: 9,
        failed: 0,
        cpu_usage_permille: 125,
        rss_bytes: 104_857_600,
        throughput_bytes_per_sec: 1_500_000,
      }),
    ).toEqual({
      pending: 0,
      running: 1,
      done: 9,
      failed: 0,
      cpu_usage_permille: 125,
      rss_bytes: 104_857_600,
      throughput_bytes_per_sec: 1_500_000,
    });
  });

  it("treats the system-metrics fields as optional (pre-v11 daemon)", () => {
    const wire = validateProgressSnapshot("daemon_progress", {
      pending: 1,
      running: 0,
      done: 0,
      failed: 0,
    });
    expect(wire.cpu_usage_permille).toBeUndefined();
    expect(wire.rss_bytes).toBeUndefined();
    expect(wire.throughput_bytes_per_sec).toBeUndefined();
  });

  it("throws when a system-metrics field is the wrong type", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        rss_bytes: "lots",
      }),
    ).toThrow(IpcValidationError);
  });

  it("mirrors the partial-clip pass counts when present", () => {
    const wire = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
      partial_pending: 5,
      partial_running: 2,
      partial_done: 3,
    });
    expect(wire.partial_pending).toBe(5);
    expect(wire.partial_running).toBe(2);
    expect(wire.partial_done).toBe(3);
  });

  it("treats partial_done as optional (pre-v22 daemon)", () => {
    const wire = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
      partial_pending: 5,
      partial_running: 2,
    });
    expect(wire.partial_done).toBeUndefined();
  });

  it("throws when partial_done is the wrong type", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        partial_done: "three",
      }),
    ).toThrow(IpcValidationError);
  });

  it("passes through the partial_skipped reason→count map when present", () => {
    const wire = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
      partial_skipped: { "unsupported-codec": 2, "duration-cap": 1 },
    });
    expect(wire.partial_skipped).toEqual({
      "unsupported-codec": 2,
      "duration-cap": 1,
    });
  });

  it("treats partial_skipped as optional (pre-v24 daemon)", () => {
    const wire = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
    });
    expect(wire.partial_skipped).toBeUndefined();
  });

  it("throws when a partial_skipped value is not a number", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        partial_skipped: { "unsupported-codec": "two" },
      }),
    ).toThrow(IpcValidationError);
  });

  it("passes through partial_failed when present, optional otherwise", () => {
    const withField = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 1,
      partial_failed: 3,
    });
    expect(withField.partial_failed).toBe(3);

    const without = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
    });
    expect(without.partial_failed).toBeUndefined();
  });

  it("passes through groups_revision when present, optional otherwise", () => {
    const withField = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
      groups_revision: 42,
    });
    expect(withField.groups_revision).toBe(42);

    const without = validateProgressSnapshot("daemon_progress", {
      pending: 0,
      running: 0,
      done: 100,
      failed: 0,
    });
    expect(without.groups_revision).toBeUndefined();
  });

  it("throws when partial_failed is not a number", () => {
    expect(() =>
      validateProgressSnapshot("daemon_progress", {
        pending: 0,
        running: 0,
        done: 0,
        failed: 0,
        partial_failed: "three",
      }),
    ).toThrow(IpcValidationError);
  });
});


describe("validateGroupStats", () => {
  it("accepts a valid payload", () => {
    expect(
      validateGroupStats("group_stats", { group_count: 7, reclaimable_bytes: 1024 }),
    ).toEqual({ group_count: 7, reclaimable_bytes: 1024 });
  });

  it("throws naming 'group_count' when absent", () => {
    try {
      validateGroupStats("group_stats", { reclaimable_bytes: 0 });
      expect.fail("expected throw");
    } catch (e) {
      expect(e).toBeInstanceOf(IpcValidationError);
      expect((e as IpcValidationError).field).toBe("group_count");
    }
  });
});


describe("validateClusterStats", () => {
  it("accepts a valid payload", () => {
    expect(
      validateClusterStats("cluster_stats", {
        cluster_count: 3,
        reclaimable_bytes: 500,
      }),
    ).toEqual({ cluster_count: 3, reclaimable_bytes: 500 });
  });
});


describe("validateGroupSummaryArray", () => {
  it("accepts a valid array", () => {
    const result = validateGroupSummaryArray("list_groups", [
      {
        group_id: 1,
        trust: "Exact",
        best_file_id: 10,
        member_count: 2,
        intro_outro: false,
      },
    ]);
    expect(result).toHaveLength(1);
    expect(result[0].trust).toBe("Exact");
  });

  it("allows best_file_id to be null", () => {
    expect(() =>
      validateGroupSummaryArray("list_groups", [
        {
          group_id: 1,
          trust: "Possible",
          best_file_id: null,
          member_count: 2,
          intro_outro: true,
        },
      ]),
    ).not.toThrow();
  });

  it("throws when trust is missing", () => {
    expect(() =>
      validateGroupSummaryArray("list_groups", [
        { group_id: 1, best_file_id: null, member_count: 2 },
      ]),
    ).toThrow(IpcValidationError);
  });

  it("throws when root is not an array", () => {
    expect(() =>
      validateGroupSummaryArray("list_groups", { group_id: 1 }),
    ).toThrow(IpcValidationError);
  });
});


describe("validateFileDetailArray", () => {
  const validDetail = {
    file_id: 42,
    path: "/v/a.mp4",
    size_bytes: 1000,
    width: 1920,
    height: 1080,
    duration_ms: 60000,
    bitrate_bps: 5000000,
    codec: "h264",
    container: "mp4",
    is_best: true,
    thumbnail: null,
  };

  it("accepts a valid array with nullable fields as null", () => {
    const result = validateFileDetailArray("list_group_detail", [validDetail]);
    expect(result[0].file_id).toBe(42);
    expect(result[0].thumbnail).toBeNull();
  });

  it("accepts nullable columns as null (unprobed file)", () => {
    expect(() =>
      validateFileDetailArray("list_group_detail", [
        {
          ...validDetail,
          width: null,
          height: null,
          duration_ms: null,
          bitrate_bps: null,
          codec: null,
          container: null,
        },
      ]),
    ).not.toThrow();
  });

  it("throws when file_id is missing", () => {
    const { file_id: _dropped, ...rest } = validDetail;
    expect(() =>
      validateFileDetailArray("list_group_detail", [rest]),
    ).toThrow(IpcValidationError);
  });

  it("throws when is_best is not a boolean", () => {
    expect(() =>
      validateFileDetailArray("list_group_detail", [
        { ...validDetail, is_best: "yes" },
      ]),
    ).toThrow(IpcValidationError);
  });
});


describe("validateClusterMemberDetailArray", () => {
  const validMember = {
    file: {
      file_id: 1,
      path: "/a.mp4",
      size_bytes: 100,
      width: null,
      height: null,
      duration_ms: null,
      bitrate_bps: null,
      codec: null,
      container: null,
      is_best: false,
      thumbnail: null,
    },
    trust: "Exact",
    group_id: 5,
  };

  it("accepts a valid member detail array", () => {
    const result = validateClusterMemberDetailArray("cluster_detail", [validMember]);
    expect(result[0].trust).toBe("Exact");
    expect(result[0].group_id).toBe(5);
  });

  it("throws when group_id is missing", () => {
    const { group_id: _dropped, ...rest } = validMember;
    expect(() =>
      validateClusterMemberDetailArray("cluster_detail", [rest]),
    ).toThrow(IpcValidationError);
  });
});


describe("validateDeleteResult", () => {
  it("accepts a valid result without reject_code (pre-v12 daemon)", () => {
    expect(
      validateDeleteResult("delete_files", {
        ok: true,
        removed_file_ids: [1, 2],
        reclaimed_bytes: 500,
        detail: "done",
      }),
    ).toEqual({
      ok: true,
      removed_file_ids: [1, 2],
      reclaimed_bytes: 500,
      detail: "done",
      reject_code: null,
    });
  });

  it("accepts a guard rejection with reject_code (v12+ daemon)", () => {
    expect(
      validateDeleteResult("delete_files", {
        ok: false,
        removed_file_ids: [],
        reclaimed_bytes: 0,
        detail: "",
        reject_code: "BEST_UNCONFIRMED",
      }),
    ).toEqual({
      ok: false,
      removed_file_ids: [],
      reclaimed_bytes: 0,
      detail: "",
      reject_code: "BEST_UNCONFIRMED",
    });
  });

  it("throws when ok is not a boolean", () => {
    expect(() =>
      validateDeleteResult("delete_files", {
        ok: "yes",
        removed_file_ids: [],
        reclaimed_bytes: 0,
        detail: "",
      }),
    ).toThrow(IpcValidationError);
  });
});


describe("validateUndoResult", () => {
  it("accepts ok:true with ids", () => {
    expect(
      validateUndoResult("undo_last_delete", {
        ok: true,
        group_id: 7,
        restored_file_ids: [2, 3],
        missing_paths: [],
        detail: "복원됨",
      }),
    ).toEqual({
      ok: true,
      group_id: 7,
      restored_file_ids: [2, 3],
      missing_paths: [],
      detail: "복원됨",
    });
  });

  it("accepts ok:false with null group_id", () => {
    expect(() =>
      validateUndoResult("undo_last_delete", {
        ok: false,
        group_id: null,
        restored_file_ids: [],
        missing_paths: [],
        detail: "없음",
      }),
    ).not.toThrow();
  });

  it("throws when missing_paths is not an array", () => {
    expect(() =>
      validateUndoResult("undo_last_delete", {
        ok: false,
        group_id: null,
        restored_file_ids: [],
        missing_paths: "nope",
        detail: "",
      }),
    ).toThrow(IpcValidationError);
  });
});


describe("validateClipOverlapArray", () => {
  it("accepts a valid overlap array", () => {
    const result = validateClipOverlapArray("partial_overlaps", [
      {
        clip_file_id: 43,
        source_file_id: 42,
        matched_scenes: 9,
        clip_scenes: 12,
        start_ms: 0,
        end_ms: 60000,
        clip_start_ms: 0,
        clip_end_ms: 60000,
        intro_outro: false,
      },
    ]);
    expect(result[0].clip_file_id).toBe(43);
    expect(result[0].clip_start_ms).toBe(0);
    expect(result[0].clip_end_ms).toBe(60000);
    expect(result[0].intro_outro).toBe(false);
  });

  it("throws when clip_end_ms is missing (v18 field)", () => {
    expect(() =>
      validateClipOverlapArray("partial_overlaps", [
        {
          clip_file_id: 1,
          source_file_id: 2,
          matched_scenes: 3,
          clip_scenes: 5,
          start_ms: 0,
          end_ms: 60000,
          clip_start_ms: 0,
        },
      ]),
    ).toThrow(IpcValidationError);
  });

  it("accepts an empty array", () => {
    expect(validateClipOverlapArray("partial_overlaps", [])).toEqual([]);
  });

  it("throws when end_ms is missing", () => {
    expect(() =>
      validateClipOverlapArray("partial_overlaps", [
        {
          clip_file_id: 1,
          source_file_id: 2,
          matched_scenes: 3,
          clip_scenes: 5,
          start_ms: 0,
        },
      ]),
    ).toThrow(IpcValidationError);
  });
});


describe("validateFailedTaskArray", () => {
  it("accepts valid rows", () => {
    const result = validateFailedTaskArray("failed_tasks", [
      { task_id: 1, path: "/a.mp4", reason: "err", attempts: 3 },
    ]);
    expect(result[0].task_id).toBe(1);
  });

  it("throws when reason is not a string", () => {
    expect(() =>
      validateFailedTaskArray("failed_tasks", [
        { task_id: 1, path: "/a.mp4", reason: 99, attempts: 1 },
      ]),
    ).toThrow(IpcValidationError);
  });
});


describe("validateCrossGroupConflictArray", () => {
  it("accepts a valid conflict array", () => {
    const result = validateCrossGroupConflictArray("cross_group_conflicts", [
      {
        file_id: 42,
        path: "/v/shared.mp4",
        memberships: [
          { group_id: 1, trust: "Exact", is_best: true },
          { group_id: 9, trust: "Possible", is_best: false },
        ],
      },
    ]);
    expect(result[0].file_id).toBe(42);
    expect(result[0].memberships).toHaveLength(2);
  });

  it("throws when a membership is_best is missing", () => {
    expect(() =>
      validateCrossGroupConflictArray("cross_group_conflicts", [
        {
          file_id: 1,
          path: "/v/x.mp4",
          memberships: [{ group_id: 1, trust: "Exact" }],
        },
      ]),
    ).toThrow(IpcValidationError);
  });
});


describe("validateWireSettings", () => {
  const validSettings = {
    scan_folders: ["/a", "/b"],
    background_enabled: true,
    auto_index: false,
    exclude_rules: ["node_modules"],
    run_on_boot: false,
    cpu_throttle: "balanced",
    best_copy_mode: "archival",
  };

  it("accepts a valid settings payload", () => {
    expect(validateWireSettings("get_settings", validSettings)).toEqual(
      validSettings,
    );
  });

  it("carries the v13 worker fields when present (idle_worker_count / cpu_cores)", () => {
    const v = validateWireSettings("get_settings", {
      ...validSettings,
      idle_worker_count: 4,
      cpu_cores: 16,
    });
    expect(v.idle_worker_count).toBe(4);
    expect(v.cpu_cores).toBe(16);
  });

  it("accepts a null idle_worker_count (auto) and an omitted cpu_cores", () => {
    const v = validateWireSettings("get_settings", {
      ...validSettings,
      idle_worker_count: null,
    });
    expect(v.idle_worker_count).toBeNull();
    expect(v.cpu_cores).toBeUndefined();
  });

  it("throws when idle_worker_count is neither number nor null", () => {
    expect(() =>
      validateWireSettings("get_settings", {
        ...validSettings,
        idle_worker_count: "lots",
      }),
    ).toThrow(IpcValidationError);
  });

  it("ignores unknown extra fields (forward-compat / additive protocol)", () => {
    expect(() =>
      validateWireSettings("get_settings", {
        ...validSettings,
        new_field_in_future_version: 42,
      }),
    ).not.toThrow();
  });

  it("throws IpcValidationError when background_enabled is not boolean", () => {
    expect(() =>
      validateWireSettings("get_settings", {
        ...validSettings,
        background_enabled: "yes",
      }),
    ).toThrow(IpcValidationError);
    try {
      validateWireSettings("get_settings", {
        ...validSettings,
        background_enabled: "yes",
      });
    } catch (e) {
      expect((e as IpcValidationError).field).toBe("background_enabled");
      expect((e as IpcValidationError).command).toBe("get_settings");
    }
  });

  it("throws when scan_folders is not an array", () => {
    expect(() =>
      validateWireSettings("get_settings", { ...validSettings, scan_folders: "nope" }),
    ).toThrow(IpcValidationError);
  });

  it("throws when cpu_throttle is not a string", () => {
    expect(() =>
      validateWireSettings("get_settings", { ...validSettings, cpu_throttle: 99 }),
    ).toThrow(IpcValidationError);
  });
});
