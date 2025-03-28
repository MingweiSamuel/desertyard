//! Cloudflare worker.
#![warn(missing_docs)]
#![feature(once_cell_try)]

mod init;

use std::cell::LazyCell;
use std::collections::BTreeMap;

use futures::future::{join_all, try_join_all};
use web_time::SystemTime;
use worker::{
    Context, Data, Env, HttpMetadata, Method, Object, Request, Response, Result, ScheduleContext,
    ScheduledEvent, event,
};

/// R2 key for the CCTV images list, pre-computed.
pub const R2_IMGS_JSON: &str = "imgs.json";

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

    // Pre-compute the list of images for each CCTV camera and store it in R2 as JSON.
    {
        let bucket = env.bucket("CCTV_BUCKET").unwrap();
        let tasks = cctv_urls.iter().map(|(cctv_name, _cctv_url)| {
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
                // Sort objects by upload time (oldest first).
                objects.sort_unstable_by_key(|obj| obj.uploaded().as_millis());

                Result::Ok((
                    cctv_name,
                    objects.iter().map(Object::key).collect::<Vec<_>>(),
                ))
            }
        });
        let dict = try_join_all(tasks)
            .await
            .unwrap()
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        bucket
            .put(
                R2_IMGS_JSON,
                Data::Bytes(serde_json::to_vec(&dict).unwrap()),
            )
            .http_metadata(HttpMetadata {
                cache_control: Some("no-cache".to_owned()),
                ..Default::default()
            })
            .execute()
            .await
            .unwrap();
    }
}

/// Cloudflare fetch request handler.
#[event(fetch)]
pub async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    init::logging();

    // Ensure we only handle GET requests.
    if Method::Get != req.method() {
        return Response::error("Method not allowed", 405);
    }

    match req.path().strip_prefix("/") {
        Some("imgs") => {
            // Serve the pre-computed list of CCTV images.
            let bucket = env.bucket("CCTV_BUCKET")?;

            let resp = Response::builder()
                .with_header("Access-Control-Allow-Methods", "GET")?
                .with_header("Access-Control-Allow-Origin", "*")?
                .with_header("Access-Control-Max-Age", "86400")?
                .with_header("Cache-Control", "no-cache")?;

            let Some(object) = bucket.get(R2_IMGS_JSON).execute().await? else {
                return Ok(resp.with_status(204).empty());
            };
            let bytes = object.body().unwrap().bytes().await.unwrap();
            Ok(resp
                .with_header("Content-Type", "application/json")?
                .fixed(bytes))
        }
        _ => Response::error(format!("Path not found: {:?}", req.path()), 404),
    }
}
