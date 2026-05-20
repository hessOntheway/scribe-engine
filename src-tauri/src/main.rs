use std::net::TcpListener;
use std::sync::OnceLock;
use std::thread;

static API_BASE_URL: OnceLock<String> = OnceLock::new();

#[tauri::command]
fn get_api_base_url() -> Result<String, String> {
    API_BASE_URL
        .get()
        .cloned()
        .ok_or_else(|| "local API server has not started".to_string())
}

fn start_backend() -> anyhow::Result<String> {
    dotenvy::dotenv().ok();

    let ask_app = my_claw::ask_app_from_env(6)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let api_base_url = format!("http://{addr}");

    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("error: failed to build desktop web runtime: {error}");
                return;
            }
        };

        runtime.block_on(async move {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("error: failed to adopt desktop web listener: {error}");
                    return;
                }
            };
            if let Err(error) = my_claw::web::serve_api_listener(ask_app, listener).await {
                eprintln!("error: desktop API server exited: {error:#}");
            }
        });
    });

    Ok(api_base_url)
}

fn main() {
    tauri::Builder::default()
        .setup(|_app| {
            let api_base_url = start_backend().map_err(|error| {
                eprintln!("error: failed to start desktop backend: {error:#}");
                Box::<dyn std::error::Error>::from(error)
            })?;
            let _ = API_BASE_URL.set(api_base_url);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_api_base_url])
        .run(tauri::generate_context!())
        .expect("error while running Scribe Engine desktop app");
}
