use vidcull_ipc::{Action, DaemonSettings, IpcClient, Request, Response};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "on".to_owned());
    let enable = match mode.as_str() {
        "on" => true,
        "off" => false,
        other => return Err(format!("unknown mode {other:?}; expected on|off").into()),
    };

    let endpoint = std::env::var("VIDCULL_IPC").unwrap_or_else(|_| vidcull_ipc::default_endpoint());
    let mut client = IpcClient::connect(&endpoint).await?;

    let current = match client.request(&Request::GetSettings).await? {
        Response::Settings(s) => s,
        other => return Err(format!("unexpected GetSettings reply: {other:?}").into()),
    };
    let updated = DaemonSettings {
        run_on_boot: enable,
        ..current
    };
    match client
        .request(&Request::Action(Action::SetSettings(updated)))
        .await?
    {
        Response::Settings(s) => {
            println!("OK run_on_boot={}", s.run_on_boot);
            Ok(())
        }
        other => Err(format!("unexpected SetSettings reply: {other:?}").into()),
    }
}
