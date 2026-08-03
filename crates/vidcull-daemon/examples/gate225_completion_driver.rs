#![allow(clippy::too_many_lines)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vidcull_ipc::{ClusterMemberDetail, ClusterSummary, IpcClient, Request, Response};

struct Config {
    endpoint: String,
    poll_interval_ms: u64,
    stable_polls: u32,
    max_wall_secs: u64,
    cluster_page_limit: u32,
    thumbs_per_cluster: usize,
    post_complete_polls: u32,
}

impl Config {
    fn from_args() -> Self {
        let mut cfg = Self {
            endpoint: std::env::var("VIDCULL_IPC")
                .unwrap_or_else(|_| vidcull_ipc::default_endpoint()),
            poll_interval_ms: 800,
            stable_polls: 3,
            max_wall_secs: 1800,
            cluster_page_limit: 500,
            thumbs_per_cluster: 4,
            post_complete_polls: 75,
        };
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            let Some(value) = args.next() else { break };
            match flag.as_str() {
                "--endpoint" => cfg.endpoint = value,
                "--poll-interval-ms" => {
                    cfg.poll_interval_ms = value.parse().unwrap_or(cfg.poll_interval_ms);
                }
                "--stable-polls" => cfg.stable_polls = value.parse().unwrap_or(cfg.stable_polls),
                "--max-wall-secs" => {
                    cfg.max_wall_secs = value.parse().unwrap_or(cfg.max_wall_secs);
                }
                "--cluster-page-limit" => {
                    cfg.cluster_page_limit = value.parse().unwrap_or(cfg.cluster_page_limit);
                }
                "--thumbs-per-cluster" => {
                    cfg.thumbs_per_cluster = value.parse().unwrap_or(cfg.thumbs_per_cluster);
                }
                "--post-complete-polls" => {
                    cfg.post_complete_polls = value.parse().unwrap_or(cfg.post_complete_polls);
                }
                _ => {}
            }
        }
        cfg
    }
}

fn emit(phase: &str, kind: &str, duration_ms: f64, ok: bool, extra: &str) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    println!(
        "{{\"ts_ms\":{ts_ms},\"phase\":\"{phase}\",\"kind\":\"{kind}\",\
         \"duration_ms\":{duration_ms:.3},\"ok\":{ok},\"extra\":\"{extra}\"}}"
    );
}

async fn timed_request(
    client: &mut IpcClient,
    phase: &str,
    kind: &str,
    req: &Request,
) -> Option<Response> {
    let start = Instant::now();
    let result = client.request(req).await;
    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(resp) => {
            emit(phase, kind, duration_ms, true, "");
            Some(resp)
        }
        Err(err) => {
            emit(phase, kind, duration_ms, false, &format!("err={err}"));
            None
        }
    }
}

async fn timed_request_stream(
    client: &mut IpcClient,
    phase: &str,
    kind: &str,
    req: &Request,
) -> Option<Vec<Response>> {
    let start = Instant::now();
    let result = client.request_stream(req).await;
    let duration_ms = start.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(frames) => {
            emit(phase, kind, duration_ms, true, "");
            Some(frames)
        }
        Err(err) => {
            emit(phase, kind, duration_ms, false, &format!("err={err}"));
            None
        }
    }
}

async fn run_completion_burst(client: &mut IpcClient, cfg: &Config) {
    let _ = timed_request(
        client,
        "refresh",
        "cluster_stats",
        &Request::ClusterStats { trust: None },
    )
    .await;

    let summaries_resp = timed_request(
        client,
        "refresh",
        "cluster_summaries",
        &Request::ClusterSummaries {
            trust: None,
            limit: cfg.cluster_page_limit,
            offset: 0,
        },
    )
    .await;
    let summaries: Vec<ClusterSummary> = match summaries_resp {
        Some(Response::ClusterSummaries(s)) => s,
        _ => Vec::new(),
    };
    emit(
        "note",
        "driver",
        0.0,
        true,
        &format!("clusters={}", summaries.len()),
    );

    for summary in &summaries {
        let detail_resp = timed_request_stream(
            client,
            "refresh",
            "cluster_detail",
            &Request::ClusterDetail {
                cluster_id: summary.cluster_id,
            },
        )
        .await;
        let members: Vec<ClusterMemberDetail> = detail_resp
            .unwrap_or_default()
            .into_iter()
            .filter_map(|resp| match resp {
                Response::ClusterDetail(m) => Some(m),
                _ => None,
            })
            .flatten()
            .collect();
        emit(
            "note",
            "driver",
            0.0,
            true,
            &format!(
                "cluster_id={} members={}",
                summary.cluster_id,
                members.len()
            ),
        );

        for member in members.iter().take(cfg.thumbs_per_cluster) {
            let _ = timed_request(
                client,
                "thumb",
                "thumbnail",
                &Request::Thumbnail {
                    file_id: member.file.file_id,
                },
            )
            .await;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_args();
    emit(
        "note",
        "driver",
        0.0,
        true,
        &format!("start endpoint={}", cfg.endpoint),
    );

    let (mut client, daemon_version) = IpcClient::connect_negotiated(&cfg.endpoint).await?;
    emit(
        "note",
        "driver",
        0.0,
        true,
        &format!("connected daemon_version={daemon_version}"),
    );

    let wall_start = Instant::now();
    let mut stable_count: u32 = 0;
    let mut fired = false;
    let mut post_polls_remaining = cfg.post_complete_polls;

    loop {
        if wall_start.elapsed().as_secs() > cfg.max_wall_secs {
            emit(
                "note",
                "driver",
                0.0,
                false,
                "max_wall_secs exceeded before completion",
            );
            break;
        }

        let progress_resp =
            timed_request(&mut client, "poll", "progress", &Request::Progress).await;
        let (pending, running, partial_pending, partial_running) = match &progress_resp {
            Some(Response::Progress(snap)) => (
                snap.pending,
                snap.running,
                snap.partial_pending,
                snap.partial_running,
            ),
            _ => (u64::MAX, u64::MAX, u64::MAX, u64::MAX),
        };
        let _ = timed_request(
            &mut client,
            "poll",
            "cluster_stats",
            &Request::ClusterStats { trust: None },
        )
        .await;

        if pending == 0 && running == 0 && partial_pending == 0 && partial_running == 0 {
            stable_count += 1;
        } else {
            stable_count = 0;
        }

        if !fired && stable_count >= cfg.stable_polls {
            fired = true;
            emit(
                "note",
                "driver",
                0.0,
                true,
                "completion detected; firing refresh+thumbnail burst",
            );
            run_completion_burst(&mut client, &cfg).await;
        }

        if fired {
            if post_polls_remaining == 0 {
                break;
            }
            post_polls_remaining -= 1;
        }

        tokio::time::sleep(Duration::from_millis(cfg.poll_interval_ms)).await;
    }

    emit("note", "driver", 0.0, true, "driver exiting");
    Ok(())
}
