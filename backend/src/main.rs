use vitals_monitor::configuration::get_configuration;
use vitals_monitor::startup::Application;
use vitals_monitor::telemetry::{get_subscriber, init_subscriber};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = get_subscriber("vitals_monitor".into(), "info".into(), std::io::stdout);
    init_subscriber(subscriber);

    let settings = get_configuration().map_err(|e| {
        tracing::error!(error = %e, "invalid configuration, refusing to start");
        e
    })?;
    let app = Application::build(settings).await?;
    app.run_until_stopped().await?;
    Ok(())
}
