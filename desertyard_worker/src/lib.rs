//! Cloudflare worker.
#![warn(missing_docs)]
#![feature(once_cell_try)]

mod init;
use web_time::SystemTime;
use worker::{
    event, Context, Data, Env, HttpMetadata, Request, Response, Result, ScheduleContext,
    ScheduledEvent,
};

/// Image URL for CCTV camera.
pub const CCTV_URL: &str = "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv722i880atjno7thstreet/tv722i880atjno7thstreet.jpg";

/// Cron update job which uploads a CCTV image to the R2 bucket.
#[event(scheduled)]
pub async fn scheduled(event: ScheduledEvent, env: Env, ctx: ScheduleContext) {
    init::logging();

    async fn scheduled_helper(
        _event: &ScheduledEvent,
        env: &Env,
        _ctx: &ScheduleContext,
    ) -> anyhow::Result<()> {
        let start = SystemTime::now();
        let epoch_secs = start.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
        // Round (mostly down) to the nearest 5 minutes.
        let epoch_secs = 300 * ((50 + epoch_secs) / 300);

        let resp = init::client(&env)?
            .get(CCTV_URL)
            .send()
            .await?
            .error_for_status()?;
        // Image is very small and easily fits into memory.
        let bytes = resp.bytes().await?;
        let value = Data::Bytes(bytes.into());
        let _object = env
            .bucket("CCTV_BUCKET")?
            .put(format!("tv722/{}.jpg", epoch_secs), value)
            .http_metadata(HttpMetadata {
                cache_control: Some("max-age=604800, public".to_owned()),
                ..Default::default()
            })
            .execute()
            .await?;
        Ok(())
    }

    for i in 0..5 {
        match scheduled_helper(&event, &env, &ctx).await {
            Ok(()) => return,
            Err(e) => {
                log::error!("Cron job failed ({i} retries): {e}");
            }
        }
    }
    log::error!("Cron job failed!");
}

/// Cloudflare fetch request handler.
#[event(fetch)]
pub async fn fetch(_req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    init::logging();
    log::info!("Request!");
    Response::ok("Hello World!")
}
