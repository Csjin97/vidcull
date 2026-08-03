> 이 문서는 `crates/vidcull-db/src/schema/v001.sql`~`v017.sql`을 순서대로 적용한 결과를 2026-08-03 기준으로 손으로 스냅샷한 것이다. 이 저장소엔 자동 재생성 CI가 없다 — 스키마 마이그레이션을 추가/변경하면 이 문서를 수동으로 다시 맞춘다.

# vidcull DB Schema (cumulative snapshot, v001–v017)

## 1. Tables

### files — v001, extended v005
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT |
| path | TEXT | NOT NULL, UNIQUE |
| size_bytes | INTEGER | NOT NULL |
| mtime_ns | INTEGER | NOT NULL |
| inode | INTEGER | |
| content_hash | BLOB | 32-byte BLAKE3, NULL until Phase 3 hashes it |
| codec | TEXT | |
| container | TEXT | |
| duration_ms | INTEGER | |
| fps_x1000 | INTEGER | fps * 1000 (integer-only storage) |
| bitrate_bps | INTEGER | |
| width_px | INTEGER | |
| height_px | INTEGER | |
| first_seen_at | INTEGER | NOT NULL |
| last_seen_at | INTEGER | NOT NULL |
| deleted_at | INTEGER | |
| laplacian_variance | REAL | (v005) |
| dct_energy | REAL | (v005) |
| bpp | REAL | (v005) |
| encoder_tags | TEXT | (v005) |

Table option: `STRICT`. No foreign keys (root table).

### fingerprints — v001, extended v010, v012
| Column | Type | Constraints |
|---|---|---|
| file_id | INTEGER | PRIMARY KEY, FK → files(id) ON DELETE CASCADE |
| tier1_global | BLOB | NOT NULL |
| tier2_temporal | BLOB | |
| format_version | INTEGER | NOT NULL |
| created_at | INTEGER | NOT NULL |
| partial_temporal | BLOB | (v010) |
| partial_skip_reason | TEXT | (v012) |
| partial_skip_size_bytes | INTEGER | (v012) |
| partial_skip_mtime_ns | INTEGER | (v012) |

`STRICT`.

### scene_hashes — v001
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT |
| file_id | INTEGER | NOT NULL, FK → files(id) ON DELETE CASCADE |
| ts_ms | INTEGER | NOT NULL |
| phash | BLOB | NOT NULL |
| band_index | INTEGER | NOT NULL — LSH band bucket for Phase 6 lookups |

`STRICT`.

### duplicate_groups — v001, extended v015
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT |
| trust_level | TEXT | NOT NULL, CHECK IN ('EXACT','VERY_LIKELY','POSSIBLE') |
| best_file_id | INTEGER | FK → files(id) ON DELETE SET NULL |
| created_at | INTEGER | NOT NULL |
| updated_at | INTEGER | NOT NULL |
| non_transitive | INTEGER | NOT NULL DEFAULT 0 (v015) |

`STRICT`.

### duplicate_group_members — v001
| Column | Type | Constraints |
|---|---|---|
| group_id | INTEGER | NOT NULL, PK (group_id, file_id), FK → duplicate_groups(id) ON DELETE CASCADE |
| file_id | INTEGER | NOT NULL, PK (group_id, file_id), FK → files(id) ON DELETE CASCADE |

`STRICT`.

### similarity_edges — v001, extended v014, v017
| Column | Type | Constraints |
|---|---|---|
| group_id | INTEGER | NOT NULL, PK (group_id, file_a, file_b), FK → duplicate_groups(id) ON DELETE CASCADE |
| file_a | INTEGER | NOT NULL, PK, FK → files(id) ON DELETE CASCADE |
| file_b | INTEGER | NOT NULL, PK, FK → files(id) ON DELETE CASCADE |
| score_x1000 | INTEGER | NOT NULL — similarity score * 1000 |
| clip_start_ms | INTEGER | (v014) clip-side first aligned scene (ms) |
| clip_end_ms | INTEGER | (v014) clip-side last aligned scene (ms) |
| source_start_ms | INTEGER | (v014) source-side first aligned scene (ms) |
| source_end_ms | INTEGER | (v014) source-side last aligned scene (ms) |
| matched_scenes | INTEGER | (v014) clip scenes that aligned (coverage numerator) |
| clip_scenes | INTEGER | (v014) total scenes in clip (coverage denominator) |
| intro_outro | INTEGER | NOT NULL DEFAULT 0 (v017) |

Table-level `CHECK (file_a < file_b)` (canonicalizes edge ordering to dedupe). `STRICT`.

### scan_state — v001
| Column | Type | Constraints |
|---|---|---|
| root_path | TEXT | PRIMARY KEY |
| last_scan_at | INTEGER | NOT NULL |
| cursor | BLOB | opaque resume cursor (postcard) |
| files_seen | INTEGER | NOT NULL DEFAULT 0 |
| bytes_seen | INTEGER | NOT NULL DEFAULT 0 |

`STRICT`. No FKs.

### task_queue — v001, extended v009
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT |
| kind | TEXT | NOT NULL |
| state | TEXT | NOT NULL, CHECK IN ('PENDING','RUNNING','DONE','FAILED') |
| priority | INTEGER | NOT NULL DEFAULT 0 |
| payload | BLOB | |
| attempts | INTEGER | NOT NULL DEFAULT 0 |
| enqueued_at | INTEGER | NOT NULL |
| started_at | INTEGER | |
| finished_at | INTEGER | |
| last_error | TEXT | |
| size_bytes | INTEGER | NOT NULL DEFAULT 0 (v009) |

`STRICT`. No FKs.

### regroup_queue — v002
| Column | Type | Constraints |
|---|---|---|
| file_id | INTEGER | PRIMARY KEY, FK → files(id) ON DELETE CASCADE |
| enqueued_at | INTEGER | NOT NULL |

`STRICT`.

### system_metadata — v003
| Column | Type | Constraints |
|---|---|---|
| key | TEXT | PRIMARY KEY |
| value | TEXT | NOT NULL |

`STRICT`.

### daemon_settings — v004
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY, CHECK (id = 1) — singleton row |
| payload | BLOB | NOT NULL |

`STRICT`.

### partial_mih_postings — v006
| Column | Type | Constraints |
|---|---|---|
| chunk | INTEGER | NOT NULL, part of PK |
| slice_value | INTEGER | NOT NULL, part of PK |
| file_id | INTEGER | NOT NULL, part of PK, FK → files(id) ON DELETE CASCADE |
| scene_index | INTEGER | NOT NULL, part of PK |

PRIMARY KEY (chunk, slice_value, file_id, scene_index). `STRICT`.

### partial_index_files — v006
| Column | Type | Constraints |
|---|---|---|
| file_id | INTEGER | PRIMARY KEY, FK → files(id) ON DELETE CASCADE |
| scene_count | INTEGER | NOT NULL |

`STRICT`.

### delete_batches — v007, extended v008, v016
| Column | Type | Constraints |
|---|---|---|
| id | INTEGER | PRIMARY KEY AUTOINCREMENT |
| group_id | INTEGER | NOT NULL — no FK by design (batch may reference a group row that was itself dropped) |
| trust_level | TEXT | NOT NULL, CHECK IN ('EXACT','VERY_LIKELY','POSSIBLE') |
| best_file_id | INTEGER | the group's best-file pointer at delete time (no FK) |
| group_dropped | INTEGER | NOT NULL, CHECK IN (0,1) |
| mode | TEXT | NOT NULL, CHECK IN ('TRASH','PERMANENT') |
| created_at | INTEGER | NOT NULL |
| status | TEXT | NOT NULL DEFAULT 'COMMITTED', CHECK IN ('PENDING','COMMITTED') (v008) |
| non_transitive | INTEGER | NOT NULL DEFAULT 0 (v016) |

`STRICT`.

### delete_batch_files — v007
| Column | Type | Constraints |
|---|---|---|
| batch_id | INTEGER | NOT NULL, PK (batch_id, file_id), FK → delete_batches(id) ON DELETE CASCADE |
| file_id | INTEGER | NOT NULL, PK (batch_id, file_id), FK → files(id) ON DELETE CASCADE |
| path | TEXT | NOT NULL |
| role | TEXT | NOT NULL, CHECK IN ('DELETED','SURVIVOR') |

`STRICT`.

---

## 2. Indexes (by table)

| Table | Index | Columns | WHERE (partial) | Introduced |
|---|---|---|---|---|
| files | idx_files_path (UNIQUE) | path | — | v001 |
| files | idx_files_content_hash | content_hash | content_hash IS NOT NULL | v001 |
| files | idx_files_last_seen | last_seen_at | — | v001 |
| fingerprints | idx_fingerprints_file | file_id | — | v001 |
| scene_hashes | idx_scene_hashes_file | file_id | — | v001 |
| scene_hashes | idx_scene_hashes_band | band_index | — | v001 |
| duplicate_groups | idx_dup_groups_trust | trust_level, id | — | v013 |
| duplicate_group_members | idx_duplicate_group_members_file | file_id | — | v001 |
| similarity_edges | idx_similarity_edges_group | group_id | — | v001 |
| task_queue | idx_task_queue_state | state, priority DESC, enqueued_at | — | v001 |
| task_queue | idx_task_queue_dequeue | state, kind, priority DESC, enqueued_at | — | v013 |
| task_queue | idx_task_queue_active_payload | kind, payload | state IN ('PENDING','RUNNING') | v013 |
| task_queue | idx_task_queue_failed_payload | payload, size_bytes | state = 'FAILED' | v017 |
| partial_mih_postings | idx_partial_mih_postings_file | file_id | — | v006 |

Tables with no secondary indexes beyond their PRIMARY KEY: `scan_state`, `regroup_queue`, `system_metadata`, `daemon_settings`, `partial_index_files`, `delete_batches`, `delete_batch_files`.

---

## 3. Table/index introduction & modification summary

| Object | Migration history |
|---|---|
| files | v001, extended v005 (quality-metric columns) |
| fingerprints | v001, extended v010 (partial_temporal), v012 (partial_skip_* columns) |
| scene_hashes | v001 |
| duplicate_groups | v001, extended v015 (non_transitive); index added v013 |
| duplicate_group_members | v001 |
| similarity_edges | v001, extended v014 (clip/source alignment columns), v017 (intro_outro) |
| scan_state | v001 |
| task_queue | v001, extended v009 (size_bytes); indexes added v013, v017 |
| regroup_queue | v002 |
| system_metadata | v003 |
| daemon_settings | v004 |
| partial_mih_postings | v006 |
| partial_index_files | v006 |
| delete_batches | v007, extended v008 (status), v016 (non_transitive) |
| delete_batch_files | v007 |

---

## 4. 역사적으로 제거됨

없음 — v001~v017 전체에서 테이블/컬럼/인덱스가 drop되거나 rename된 적이 없다. 모든 `CREATE`/`ALTER ADD COLUMN`이 그대로 유효하다.
