//! Cloudflare worker.
#![warn(missing_docs)]
#![feature(once_cell_try)]

mod init;

use std::cell::LazyCell;
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::io::Write;

use futures::future::{join_all, try_join_all};
use web_time::SystemTime;
use worker::{
    Context, Data, Env, HttpMetadata, Request, Response, Result, ScheduleContext, ScheduledEvent,
    event,
};

/// URLs for CCTV cameras. Keys are the camera name/bucket folder, and values are the image URLs.
pub const CCTV_URLS: LazyCell<BTreeMap<&'static str, &'static str>> = LazyCell::new(|| {
    [
        ("tv721", "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv721i880at7thst/tv721i880at7thst.jpg"),
        ("tv722", "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv722i880atjno7thstreet/tv722i880atjno7thstreet.jpg"),
        ("tv726", "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv726i880atjct80/tv726i880atjct80.jpg"),
        ("tv727", "https://cwwp2.dot.ca.gov/data/d4/cctv/image/tv727i880n880atgrandav/tv727i880n880atgrandav.jpg"),
    ].into_iter().collect()
});

/// Cron update job which uploads a CCTV image to the R2 bucket.
#[event(scheduled)]
pub async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    init::logging();

    async fn transfer_cctv(env: &Env, cctv_name: &str, cctv_url: &str) -> anyhow::Result<()> {
        let start = SystemTime::now();
        let epoch_secs = start.duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
        log::info!("Fetching {cctv_name} image for time {epoch_secs}.");

        let resp = init::client()?.get(format!("{cctv_url}?{epoch_secs}")) // Prevent caching.
            .send()
            .await?
            .error_for_status()?;
        // Image is very small and easily fits into memory.
        let bytes = resp.bytes().await?;
        log::info!("Downloaded {cctv_name} image ({} kib)", bytes.len() / 1024);

        // Use the hash as the filename to avoid duplicate images.
        let filename_md5 = hex::encode(*md5::compute(bytes.as_ref()));
        let bucket_data = Data::Bytes(bytes.into());
        let bucket_key = format!("{cctv_name}/{filename_md5}.jpg");
        let _object = env
            .bucket("CCTV_BUCKET")?
            .put(bucket_key.clone(), bucket_data)
            .http_metadata(HttpMetadata {
                cache_control: Some("max-age=604800, public".to_owned()),
                ..Default::default()
            })
            .execute()
            .await?;
        log::info!("Successfully uploaded {bucket_key} to R2.");

        Ok(())
    }

    let cctv_urls = &*CCTV_URLS;
    let tasks = cctv_urls.iter().map(|(cctv_name, cctv_url)| {
        let env = &env;
        async move {
            for i in 0..5 {
                match transfer_cctv(&env, cctv_name, cctv_url).await {
                    Ok(()) => return,
                    Err(e) => {
                        log::error!("Failed to transfer {cctv_name} image ({i} retries): {e}");
                    }
                }
            }
        }
    });
    let _done = join_all(tasks).await;
}

/// Cloudflare fetch request handler.
#[event(fetch)]
pub async fn fetch(_req: Request, env: Env, _ctx: Context) -> Result<Response> {
    init::logging();
    let bucket = env.bucket("CCTV_BUCKET")?;

    let cctv_urls = &*CCTV_URLS;
    let tasks = cctv_urls.into_iter().map(|(cctv_name, _cctv_url)| {
        let bucket = &bucket;
        async move {
            let prefix = &*format!("{cctv_name}/");
            let mut objects = Vec::new();
            let mut listing = bucket.list().prefix(prefix).execute().await?;

            objects.extend(listing.objects());
            while let Some(cursor) = listing.cursor() {
                listing = bucket
                    .list()
                    .cursor(cursor)
                    .prefix(prefix)
                    .execute()
                    .await?;
                objects.extend(listing.objects());
            }
            // Sort objects by upload time (newest first).
            objects.sort_unstable_by_key(|obj| Reverse(obj.uploaded().as_millis()));

            let mut out = Vec::with_capacity(
                32 + cctv_name.as_bytes().len()
                    + objects.len() * (1 + objects.first().map_or(0, |o| o.key().as_bytes().len())),
            );
            {
                write!(&mut out, "{:?}:", cctv_name).unwrap();
                out.push(b'[');
                for object in objects {
                    write!(&mut out, "{:?},", object.key()).unwrap();
                }
                if b',' == *out.last().unwrap() {
                    out.pop(); // Remove trailing comma.
                }
                out.push(b']');
            }
            Result::Ok(out)
        }
    });
    let rows = try_join_all(tasks).await?;
    assert_ne!(0, rows.len(), "No CCTV cameras found!");

    let mut out = Vec::with_capacity(2 + rows.len() + rows.iter().map(|v| v.len()).sum::<usize>());
    {
        out.push(b'{');
        for row in rows {
            out.extend(row);
            out.push(b',');
        }
        out.pop(); // Remove trailing comma.
        out.push(b'}');
    }

    Ok(Response::builder()
        .with_header("Access-Control-Allow-Methods", "GET")?
        .with_header("Access-Control-Allow-Origin", "*")?
        .with_header("Access-Control-Max-Age", "86400")?
        .with_header("Cache-Control", "no-cache")?
        .with_header("Content-Type", "application/json")?
        .fixed(out))
}
