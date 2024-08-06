//! Cloudflare worker.
#![warn(missing_docs)]
#![feature(once_cell_try)]

mod init;

use std::io::Write;

use web_time::SystemTime;
use worker::{
    event, Context, Data, Env, HttpMetadata, Object, Request, Response, Result, ScheduleContext,
    ScheduledEvent,
};

/// Image URL for CCTV camera.
pub const CCTV_URL: &str = "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv722i880atjno7thstreet/tv722i880atjno7thstreet.jpg";
/// R2 bucket folder name.
pub const BUCKET_FOLDER: &str = "tv722";

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
        let epoch_rounded = 300 * ((50 + epoch_secs) / 300);
        log::info!("Fetching CCTV image for time {epoch_rounded} ({epoch_secs}).");

        let resp = init::client()?
            .get(format!("{CCTV_URL}?{epoch_secs}")) // Prevent caching.
            .send()
            .await?
            .error_for_status()?;
        // Image is very small and easily fits into memory.
        let bytes = resp.bytes().await?;
        log::info!("Downloaded image ({} kib)", bytes.len() / 1024);

        let value = Data::Bytes(bytes.into());
        let key = format!("{BUCKET_FOLDER}/{epoch_rounded}.jpg");
        let _object = env
            .bucket("CCTV_BUCKET")?
            .put(key.clone(), value)
            .http_metadata(HttpMetadata {
                cache_control: Some("max-age=604800, public".to_owned()),
                ..Default::default()
            })
            .execute()
            .await?;
        log::info!("Successfully uploaded {key} to R2.");

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
pub async fn fetch(_req: Request, env: Env, _ctx: Context) -> Result<Response> {
    init::logging();
    let bucket = env.bucket("CCTV_BUCKET")?;

    let mut keys = Vec::new();
    let mut objects = bucket
        .list()
        .prefix(format!("{BUCKET_FOLDER}/"))
        .execute()
        .await?;

    keys.extend(objects.objects().iter().map(Object::key));
    while let Some(cursor) = objects.cursor() {
        objects = bucket.list().cursor(cursor).execute().await?;
        keys.extend(objects.objects().iter().map(Object::key));
    }

    keys.sort_unstable();

    let mut out = Vec::with_capacity(keys.len() * keys[0].bytes().len() + 32);
    {
        out.push(b'[');
        for key in keys {
            write!(&mut out, "{:?},", key).unwrap();
        }
        out.pop(); // Remove trailing comma.
        out.push(b']');
    }

    Ok(Response::builder()
        .with_header("Access-Control-Allow-Methods", "GET")?
        .with_header("Access-Control-Allow-Origin", "*")?
        .with_header("Access-Control-Max-Age", "86400")?
        .with_header("Cache-Control", "no-cache")?
        .with_header("Content-Type", "application/json")?
        .fixed(out))
}
