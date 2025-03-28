//! Helper utilities.

use std::sync::{Once, OnceLock};

use reqwest::Client;
use web_sys::console;
use worker::{console_error, console_log};

/// Initialize [`log`] logging into Cloudflare's [`console`] logging system, if not already
/// initialized.
pub fn logging() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        {
            fn hook(info: &std::panic::PanicHookInfo) {
                console_error!("{}", info);
            }
            std::panic::set_hook(Box::new(hook));
            console_log!("[panic hook set]");
        }
        {
            struct ConsoleLog;
            static LOG: ConsoleLog = ConsoleLog;
            impl log::Log for ConsoleLog {
                fn enabled(&self, _metadata: &log::Metadata) -> bool {
                    true // TODO
                }

                fn log(&self, record: &log::Record) {
                    let method = match record.level() {
                        log::Level::Error => console::error_1,
                        log::Level::Warn => console::warn_1,
                        log::Level::Info => console::info_1,
                        log::Level::Debug => console::debug_1,
                        log::Level::Trace => console::trace_1,
                    };
                    (method)(
                        &format!(
                            "[{} {}] {}",
                            record.level(),
                            record.module_path().unwrap_or("?"),
                            record.args()
                        )
                        .into(),
                    );
                }

                fn flush(&self) {}
            }
            log::set_logger(&LOG).unwrap();
            log::set_max_level(log::LevelFilter::Trace); // TODO

            log::info!("logger set");
        }
    });
}

pub fn client() -> worker::Result<&'static Client> {
    static ONCE: OnceLock<Client> = OnceLock::new();
    ONCE.get_or_try_init(|| {
        let user_agent = format!(
            "desertyard / {}",
            option_env!("GIT_HASH").unwrap_or("localdev"),
        );
        log::info!(
            "Initializing reqwest client with user agent: {:?}",
            user_agent
        );
        let client = Client::builder()
            .user_agent(user_agent)
            .build()
            .map_err(|e| format!("Failed to build reqwest client: {}", e))?;
        Ok(client)
    })
}
