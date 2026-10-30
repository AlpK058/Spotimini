use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    io::{Read, Write},
    net::TcpListener,
    time::{Duration, Instant},
};
use url::Url;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const REDIRECT: &str = "http://127.0.0.1:8888/callback";
const SERVICE: &str = "Spotimini";

#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
}

fn random_string(bytes: usize) -> Result<String> {
    let mut buf = vec![0; bytes];
    getrandom::fill(&mut buf).map_err(|e| format!("Rastgele veri olusturulamadi: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn credential(client_id: &str) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(SERVICE, client_id)?)
}
fn remembered_client_id() -> Option<String> {
    keyring::Entry::new(SERVICE, "last-client-id")
        .ok()?
        .get_password()
        .ok()
}
fn remember_client_id(client_id: &str) -> Result<()> {
    keyring::Entry::new(SERVICE, "last-client-id")?.set_password(client_id)?;
    Ok(())
}
fn token_request(fields: &[(&str, &str)]) -> Result<Tokens> {
    Ok(ureq::post("https://accounts.spotify.com/api/token")
        .send_form(fields)?
        .into_json()?)
}
fn login(client_id: &str) -> Result<()> {
    let verifier = random_string(64)?;
    let state = random_string(32)?;
    let listener = TcpListener::bind("127.0.0.1:8888")?;
    listener.set_nonblocking(true)?;
    let mut auth = Url::parse("https://accounts.spotify.com/authorize")?;
    auth.query_pairs_mut().append_pair("client_id", client_id).append_pair("response_type", "code")
        .append_pair("redirect_uri", REDIRECT).append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge(&verifier)).append_pair("state", &state)
        .append_pair("scope", "user-library-read user-read-private user-read-email playlist-read-private playlist-read-collaborative user-read-playback-state user-modify-playback-state streaming");
    println!("Tarayicida Spotify girisi aciliyor. Acilmazsa: {auth}");
    if let Err(e) = open::that(auth.as_str()) {
        eprintln!("Tarayici acilamadi: {e}");
    }
    let started = Instant::now();
    loop {
        if started.elapsed() > Duration::from_secs(180) {
            return Err("Giris zaman asimina ugradi".into());
        }
        let (mut stream, _) = match listener.accept() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf)?;
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let callback = Url::parse(&format!("http://127.0.0.1:8888{target}"));
        let parsed = callback
            .ok()
            .filter(|u| u.path() == "/callback")
            .and_then(|u| {
                let params: std::collections::HashMap<_, _> =
                    u.query_pairs().into_owned().collect();
                if params.get("state") != Some(&state) {
                    return None;
                }
                Some((params.get("code").cloned(), params.get("error").cloned()))
            });
        let (code, error) = match parsed {
            Some(v) => v,
            None => {
                let _ = stream.write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
        };
        if let Some(error) = error {
            let _ = stream.write_all(
                b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            return Err(format!("Spotify girisi reddedildi: {error}").into());
        }
        let code = code.ok_or("Spotify yetkilendirme kodu gondermedi")?;
        let tokens = token_request(&[
            ("client_id", client_id),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", REDIRECT),
            ("code_verifier", &verifier),
        ])?;
        credential(client_id)?.set_password(
            tokens
                .refresh_token
                .as_deref()
                .ok_or("Yenileme tokeni alinmadi")?,
        )?;
        remember_client_id(client_id)?;
        let body = "Giris tamamlandi. Bu pencereyi kapatabilirsiniz.";
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body)?;
        println!("Giris tamamlandi.");
        return Ok(());
    }
}
fn access_token(client_id: &str) -> Result<String> {
    let entry = credential(client_id)?;
    let refresh = entry.get_password()?;
    let tokens = token_request(&[
        ("client_id", client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", &refresh),
    ])?;
    if let Some(new_refresh) = tokens.refresh_token {
        entry.set_password(&new_refresh)?;
    }
    remember_client_id(client_id)?;
    Ok(tokens.access_token)
}
#[derive(Clone)]
struct Track {
    id: String,
    label: String,
    image: Option<String>,
    url: Option<String>,
    playlist: bool,
}
fn search_tracks(client_id: &str, query: &str) -> Result<Vec<Track>> {
    let token = access_token(client_id)?;
    let result: serde_json::Value = ureq::get("https://api.spotify.com/v1/search")
        .set("Authorization", &format!("Bearer {token}"))
        .query("q", query)
        .query("type", "track")
        .query("limit", "10")
        .call()?
        .into_json()?;
    let items = result["tracks"]["items"]
        .as_array()
        .ok_or("Spotify arama sonucu gecersiz")?;
    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item["id"].as_str()?.to_owned();
            let label = format!(
                "{} - {}",
                item["name"].as_str().unwrap_or("?"),
                item["artists"][0]["name"].as_str().unwrap_or("?")
            );
            Some(Track {
                id,
                label,
                image: item["album"]["images"][0]["url"]
                    .as_str()
                    .map(str::to_owned),
                url: item["external_urls"]["spotify"].as_str().map(str::to_owned),
                playlist: false,
            })
        })
        .collect())
}

fn library_page(client_id: &str, section: &str, offset: usize) -> Result<Vec<Track>> {
    let token = access_token(client_id)?;
    let endpoint = match section {
        "liked" => "https://api.spotify.com/v1/me/tracks".to_owned(),
        "playlists" => "https://api.spotify.com/v1/me/playlists".to_owned(),
        id => format!("https://api.spotify.com/v1/playlists/{id}/items"),
    };
    let response = ureq::get(&endpoint)
        .set("Authorization", &format!("Bearer {token}"))
        .query("limit", "50")
        .query("offset", &offset.to_string())
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(403, _) if section == "liked" => {
                "Kutuphane izni gerekli; Ayarlar'dan Spotify ile yeniden giris yapin.".to_owned()
            }
            ureq::Error::Status(403, _) => "Spotify bu listeye erisim vermiyor.".to_owned(),
            other => other.to_string(),
        })?;
    let data: serde_json::Value = response.into_json()?;
    let items = data["items"]
        .as_array()
        .ok_or("Gecersiz kutuphane cevabi")?;
    Ok(items
        .iter()
        .filter_map(|row| {
            let playlist = section == "playlists";
            let item = if playlist {
                row
            } else {
                row.get("item")
                    .filter(|v| !v.is_null())
                    .or_else(|| row.get("track"))?
            };
            if !playlist && item["type"].as_str() != Some("track") {
                return None;
            }
            let id = item["id"].as_str()?.to_owned();
            let name = item["name"].as_str().unwrap_or("?");
            let artist = item["artists"][0]["name"].as_str().unwrap_or("");
            let label = if playlist {
                name.to_owned()
            } else {
                format!("{name} - {artist}")
            };
            let image = if playlist {
                &item["images"][0]["url"]
            } else {
                &item["album"]["images"][0]["url"]
            };
            Some(Track {
                id,
                label,
                image: image.as_str().map(str::to_owned),
                url: item["external_urls"]["spotify"].as_str().map(str::to_owned),
                playlist,
            })
        })
        .collect())
}

#[derive(Default)]
struct SoloistProcess {
    child: Option<std::process::Child>,
    closed: bool,
}

impl SoloistProcess {
    fn stop(&mut self) {
        self.closed = true;
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct DesktopApp {
    client_id: String,
    query: String,
    results: Vec<Track>,
    status: String,
    pending: Option<std::sync::mpsc::Receiver<std::result::Result<Vec<Track>, String>>>,
    pending_login: bool,
    playback: Option<tokio::sync::mpsc::UnboundedSender<PlaybackCommand>>,
    playback_status: Option<std::sync::mpsc::Receiver<PlaybackUpdate>>,
    soloist_path: String,
    soloist_key: String,
    soloist_autostart: bool,
    soloist_process: std::sync::Arc<std::sync::Mutex<SoloistProcess>>,
    now: NowPlaying,
    show_settings: bool,
    section: String,
    offset: usize,
    append_page: bool,
    has_more: bool,
    covers: std::collections::HashMap<String, Option<eframe::egui::TextureHandle>>,
    cover_tx: std::sync::mpsc::Sender<(String, Option<eframe::egui::ColorImage>)>,
    cover_rx: std::sync::mpsc::Receiver<(String, Option<eframe::egui::ColorImage>)>,
}
impl Default for DesktopApp {
    fn default() -> Self {
        let client_id = env::var("SPOTIMINI_CLIENT_ID")
            .ok()
            .filter(|id| !id.trim().is_empty())
            .or_else(remembered_client_id)
            .unwrap_or_default();
        let has_session = !client_id.is_empty()
            && credential(&client_id)
                .and_then(|entry| entry.get_password().map_err(Into::into))
                .is_ok();
        let (cover_tx, cover_rx) = std::sync::mpsc::channel();
        Self {
            client_id,
            query: String::new(),
            results: Vec::new(),
            status: if has_session {
                "Kayitli oturum bulundu; arama yapabilirsiniz."
            } else {
                "Spotify client ID girin ve giris yapin."
            }
            .into(),
            pending: None,
            pending_login: false,
            playback: None,
            playback_status: None,
            soloist_autostart: true,
            soloist_process: Default::default(),
            now: NowPlaying::default(),
            show_settings: false,
            section: "search".into(),
            offset: 0,
            append_page: false,
            has_more: false,
            covers: Default::default(),
            cover_tx,
            cover_rx,
            soloist_path: keyring::Entry::new(SERVICE, "soloist-path")
                .ok()
                .and_then(|e| e.get_password().ok())
                .unwrap_or_default(),
            soloist_key: keyring::Entry::new(SERVICE, "soloist-key")
                .ok()
                .and_then(|e| e.get_password().ok())
                .unwrap_or_default(),
        }
    }
}
impl Drop for DesktopApp {
    fn drop(&mut self) {
        if let Ok(mut process) = self.soloist_process.lock() {
            process.stop();
        }
    }
}
impl DesktopApp {
    fn load_library(&mut self, ctx: &eframe::egui::Context, section: String, append: bool) {
        if self.pending.is_some() {
            return;
        }
        let offset = if append { self.offset + 50 } else { 0 };
        self.section = section.clone();
        self.offset = offset;
        self.append_page = append;
        self.pending_login = false;
        let id = self.client_id.trim().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending = Some(rx);
        self.status = "Yukleniyor...".into();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = library_page(&id, &section, offset).map_err(|e| e.to_string());
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    fn cover(
        &mut self,
        ctx: &eframe::egui::Context,
        url: &str,
    ) -> Option<eframe::egui::TextureHandle> {
        if let Some(texture) = self.covers.get(url) {
            return texture.clone();
        }
        if self.covers.len() >= 128 {
            self.covers.clear();
        }
        self.covers.insert(url.to_owned(), None);
        let url = url.to_owned();
        let tx = self.cover_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let image = (|| -> Option<eframe::egui::ColorImage> {
                let response = ureq::get(&url).call().ok()?;
                let mut bytes = Vec::new();
                response
                    .into_reader()
                    .take(3_000_000)
                    .read_to_end(&mut bytes)
                    .ok()?;
                let image = image::load_from_memory(&bytes)
                    .ok()?
                    .thumbnail(128, 128)
                    .to_rgba8();
                let size = [image.width() as usize, image.height() as usize];
                Some(eframe::egui::ColorImage::from_rgba_unmultiplied(
                    size, &image,
                ))
            })();
            let _ = tx.send((url, image));
            ctx.request_repaint();
        });
        None
    }
    fn start(&mut self, ctx: &eframe::egui::Context, action: &'static str) {
        let id = self.client_id.trim().to_owned();
        if id.is_empty() {
            self.status = "Spotify client ID gerekli.".into();
            return;
        }
        let query = self.query.trim().to_owned();
        if action == "search" && query.is_empty() {
            self.status = "Bir arama yazin.".into();
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = ctx.clone();
        self.pending = Some(rx);
        self.pending_login = action == "login";
        self.status = if action == "login" {
            "Tarayicida Spotify girisini tamamlayin..."
        } else {
            "Araniyor..."
        }
        .into();
        std::thread::spawn(move || {
            let result = if action == "login" {
                login(&id).map(|_| Vec::new())
            } else {
                search_tracks(&id, &query)
            };
            let _ = tx.send(result.map_err(|e| e.to_string()));
            ctx.request_repaint();
        });
    }
}
#[derive(Default)]
struct NowPlaying {
    title: String,
    artist: String,
    playing: bool,
    position: u64,
    duration: u64,
    volume: u8,
    image: Option<String>,
}
enum PlaybackUpdate {
    Message(String),
    State(NowPlaying),
}
fn time_label(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms / 1000) % 60)
}
fn parse_playback(value: &serde_json::Value) -> NowPlaying {
    let item = &value["item"]["decorations"];
    NowPlaying {
        title: item["identity"]["name"]
            .as_str()
            .unwrap_or("Bir sarki secin")
            .to_owned(),
        artist: item["creators"][0]["entity"]["decorations"]["identity"]["name"]
            .as_str()
            .unwrap_or("")
            .to_owned(),
        playing: value["status"].as_str() == Some("playing"),
        position: value["position"]["position_ms"].as_u64().unwrap_or(0),
        duration: item["playback"]["duration_ms"].as_u64().unwrap_or(0),
        volume: value["volume"].as_u64().unwrap_or(50).min(100) as u8,
        image: item["visual_identity"]["cover"][0]["url"]
            .as_str()
            .map(str::to_owned),
    }
}
enum PlaybackCommand {
    Play(Track),
    Pause,
    Resume,
    Next,
    Previous,
    Seek(u64),
    Volume(u8),
}
impl DesktopApp {
    fn start_soloist(&mut self, ctx: &eframe::egui::Context) {
        if self.playback.as_ref().is_some_and(|tx| !tx.is_closed()) {
            self.status = "Soloist zaten acik.".into();
            return;
        }
        let path = self.soloist_path.trim().to_owned();
        let key = self.soloist_key.trim().to_owned();
        if key.is_empty() {
            self.status = "Soloist API anahtarini girin.".into();
            return;
        }
        for (name, value) in [("soloist-path", &path), ("soloist-key", &key)] {
            if let Err(e) =
                keyring::Entry::new(SERVICE, name).and_then(|entry| entry.set_password(value))
            {
                self.status = format!("Ayar kaydedilemedi: {e}");
                return;
            }
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (status_tx, status_rx) = std::sync::mpsc::channel();
        self.playback = Some(tx);
        self.playback_status = Some(status_rx);
        let soloist_process = self.soloist_process.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            let notify = |message: String| {
                let _ = status_tx.send(PlaybackUpdate::Message(message));
                ctx.request_repaint();
            };
            let app_dir = match std::env::var_os("LOCALAPPDATA") {
                Some(v) => std::path::PathBuf::from(v)
                    .join("Spotimini")
                    .join("soloist"),
                None => {
                    notify("LOCALAPPDATA bulunamadi.".into());
                    return;
                }
            };
            if let Err(e) = std::fs::create_dir_all(&app_dir) {
                notify(format!("Klasor hatasi: {e}"));
                return;
            }
            let path = if path.is_empty() {
                let install_dir = app_dir.join("player");
                let exe = install_dir.join("soloist.exe");
                if !exe.is_file() {
                    notify("Oynatici indiriliyor...".into());
                    let download = (|| -> Result<()> {
                        std::fs::create_dir_all(&install_dir)?;
                        let archive = app_dir.join("soloist.zip");
                        let response = ureq::get("https://soloist-builds.spotifycdn.com/soloist_release_windows_x86_64.zip").call()?;
                        let mut reader = response.into_reader().take(100 * 1024 * 1024 + 1);
                        let mut file = std::fs::File::create(&archive)?;
                        if std::io::copy(&mut reader, &mut file)? > 100 * 1024 * 1024 {
                            return Err("Soloist arsivi cok buyuk".into());
                        }
                        drop(file);
                        let result = std::process::Command::new("tar.exe")
                            .arg("-xf")
                            .arg(&archive)
                            .arg("-C")
                            .arg(&install_dir)
                            .creation_flags(CREATE_NO_WINDOW)
                            .output()?;
                        if !result.status.success() || !exe.is_file() {
                            return Err("Soloist arsivi acilamadi".into());
                        }
                        std::fs::remove_file(archive)?;
                        Ok(())
                    })();
                    if let Err(e) = download {
                        notify(format!("Oynatici indirilemedi: {e}"));
                        return;
                    }
                }
                exe.to_string_lossy().into_owned()
            } else {
                path
            };
            let mut process = match soloist_process.lock() {
                Ok(process) => process,
                Err(_) => return,
            };
            if process.closed {
                return;
            }
            let child = match std::process::Command::new(&path)
                .args([
                    "--device-name",
                    "Spotimini Soloist",
                    "--api-key",
                    &key,
                    "--ws",
                    "127.0.0.1:0",
                    "--data-dir",
                ])
                .arg(&app_dir)
                .args(["--cache-size", "100"])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    notify(format!("Soloist acilamadi: {e}"));
                    return;
                }
            };
            process.child = Some(child);
            drop(process);
            notify(
                "Soloist aciliyor. Telefonda Spotify cihazlarindan Spotimini Soloist'i secin."
                    .into(),
            );
            let mut last_poll = Instant::now() - Duration::from_secs(3);
            loop {
                let command = match rx.try_recv() {
                    Ok(command) => Some(command),
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
                };
                let child_status = match soloist_process.lock() {
                    Ok(process) if process.closed => break,
                    Ok(mut process) => match process.child.as_mut() {
                        Some(child) => child.try_wait(),
                        None => break,
                    },
                    Err(_) => break,
                };
                match child_status {
                    Ok(Some(status)) => {
                        notify(format!(
                            "Soloist kapandi: {status}. Anahtari ve surumu kontrol edin."
                        ));
                        break;
                    }
                    Err(e) => {
                        notify(format!("Soloist hatasi: {e}"));
                        break;
                    }
                    _ => {}
                }
                if command.is_none() {
                    if last_poll.elapsed() >= Duration::from_secs(2) {
                        last_poll = Instant::now();
                        if let Ok(output) = std::process::Command::new(&path)
                            .arg("ctl")
                            .arg("--data-dir")
                            .arg(&app_dir)
                            .args(["now", "--json"])
                            .creation_flags(CREATE_NO_WINDOW)
                            .output()
                        {
                            if output.status.success() {
                                if let Ok(value) =
                                    serde_json::from_slice::<serde_json::Value>(&output.stdout)
                                {
                                    let _ = status_tx
                                        .send(PlaybackUpdate::State(parse_playback(&value)));
                                    ctx.request_repaint();
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let mut args = vec![
                    "ctl".to_string(),
                    "--data-dir".to_string(),
                    app_dir.to_string_lossy().to_string(),
                ];
                let label = match command.unwrap() {
                    PlaybackCommand::Play(track) => {
                        args.push("play".into());
                        args.push(format!("spotify:track:{}", track.id));
                        format!("Oynatma komutu: {}", track.label)
                    }
                    PlaybackCommand::Pause => {
                        args.push("pause".into());
                        "Duraklatma komutu gonderildi.".into()
                    }
                    PlaybackCommand::Resume => {
                        args.push("play".into());
                        "Devam komutu gonderildi.".into()
                    }
                    PlaybackCommand::Next => {
                        args.push("next".into());
                        "Sonraki sarki.".into()
                    }
                    PlaybackCommand::Previous => {
                        args.push("prev".into());
                        "Onceki sarki.".into()
                    }
                    PlaybackCommand::Seek(ms) => {
                        args.extend(["seek".into(), ms.to_string()]);
                        "Konum degistirildi.".into()
                    }
                    PlaybackCommand::Volume(volume) => {
                        args.extend(["volume".into(), volume.to_string()]);
                        "Ses ayarlandi.".into()
                    }
                };
                match std::process::Command::new(&path)
                    .args(&args)
                    .creation_flags(CREATE_NO_WINDOW)
                    .output()
                {
                    Ok(output) if output.status.success() => notify(label),
                    Ok(output) => notify(format!(
                        "Soloist: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )),
                    Err(e) => notify(format!("Soloist kontrol hatasi: {e}")),
                }
            }
            if let Ok(mut process) = soloist_process.lock() {
                if let Some(mut child) = process.child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            };
        });
    }
    fn play_track(&mut self, _ctx: &eframe::egui::Context, track: Track) {
        if let Some(tx) = &self.playback {
            if tx.send(PlaybackCommand::Play(track)).is_ok() {
                self.status = "Oynatma komutu gonderiliyor...".into();
                return;
            }
        }
        self.status = "Once Soloist'i baslatin ve telefondan eslestirin.".into();
    }
}
impl eframe::App for DesktopApp {
    fn update(&mut self, ctx: &eframe::egui::Context, _frame: &mut eframe::Frame) {
        if self.soloist_autostart {
            self.soloist_autostart = false;
            if !self.soloist_key.is_empty() {
                self.start_soloist(ctx);
            }
        }
        if let Some(rx) = &self.playback_status {
            while let Ok(message) = rx.try_recv() {
                match message {
                    PlaybackUpdate::Message(message) => self.status = message,
                    PlaybackUpdate::State(state) => {
                        if !ctx.input(|i| i.pointer.primary_down()) {
                            self.now = state;
                        }
                    }
                }
            }
        }
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(Ok(items)) => {
                    self.status = if self.pending_login {
                        "Giris tamamlandi; arama yapabilirsiniz.".into()
                    } else if items.is_empty() {
                        "Sonuc bulunamadi.".into()
                    } else {
                        format!("{} sonuc bulundu.", items.len())
                    };
                    self.has_more = items.len() == 50;
                    if self.append_page {
                        self.results.extend(items);
                    } else {
                        self.results = items;
                    }
                    self.append_page = false;
                    self.pending = None;
                }
                Ok(Err(e)) => {
                    self.status = format!("Hata: {e}");
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.status = "Islem beklenmedik sekilde durdu.".into();
                    self.pending = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        while let Ok((url, image)) = self.cover_rx.try_recv() {
            if let Some(image) = image {
                if self.covers.contains_key(&url) {
                    self.covers.insert(
                        url,
                        Some(ctx.load_texture("cover", image, Default::default())),
                    );
                }
            }
        }
        use eframe::egui::{self, Color32, RichText, Stroke, Vec2};
        ctx.set_visuals(egui::Visuals::dark());
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(20, 23, 24))
                    .inner_margin(16.0),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("SPOTIMINI")
                            .size(19.0)
                            .strong()
                            .color(Color32::from_rgb(95, 225, 150)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Ayarlar").clicked() {
                            self.show_settings = !self.show_settings;
                        }
                    });
                });
            });
        let now_cover = self.now.image.clone().and_then(|url| self.cover(ctx, &url));
        egui::TopBottomPanel::bottom("player")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(25, 29, 30))
                    .inner_margin(16.0),
            )
            .show(ctx, |ui| {
                ui.set_min_height(125.0);
                ui.horizontal(|ui| {
                    if let Some(texture) = &now_cover {
                        ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::splat(52.0)));
                    }
                    ui.vertical(|ui| {
                        ui.set_min_width(170.0);
                        ui.label(
                            RichText::new(if self.now.title.is_empty() {
                                "Bir sarki secin"
                            } else {
                                &self.now.title
                            })
                            .strong()
                            .size(16.0),
                        );
                        ui.label(RichText::new(&self.now.artist).color(Color32::GRAY));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut volume = self.now.volume;
                        let response = ui.add(
                            egui::Slider::new(&mut volume, 0..=100)
                                .show_value(false)
                                .clamping(egui::SliderClamping::Always),
                        );
                        ui.label(format!("Ses {volume}%"));
                        if response.changed() {
                            self.now.volume = volume;
                            if let Some(tx) = &self.playback {
                                let _ = tx.send(PlaybackCommand::Volume(volume));
                            }
                        }
                    });
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("|<").on_hover_text("Onceki").clicked() {
                        if let Some(tx) = &self.playback {
                            let _ = tx.send(PlaybackCommand::Previous);
                        }
                    }
                    let text = if self.now.playing {
                        "Duraklat"
                    } else {
                        "Oynat"
                    };
                    if ui
                        .add(
                            egui::Button::new(RichText::new(text).strong())
                                .fill(Color32::from_rgb(55, 169, 100))
                                .min_size(Vec2::new(82.0, 30.0)),
                        )
                        .clicked()
                    {
                        if let Some(tx) = &self.playback {
                            let _ = tx.send(if self.now.playing {
                                PlaybackCommand::Pause
                            } else {
                                PlaybackCommand::Resume
                            });
                        }
                    }
                    if ui.button(">|").on_hover_text("Sonraki").clicked() {
                        if let Some(tx) = &self.playback {
                            let _ = tx.send(PlaybackCommand::Next);
                        }
                    }
                    ui.label(time_label(self.now.position));
                    let mut position = self.now.position.min(self.now.duration);
                    let response = ui.add_enabled(
                        self.now.duration > 0,
                        egui::Slider::new(&mut position, 0..=self.now.duration.max(1))
                            .show_value(false),
                    );
                    if response.drag_stopped() || (response.changed() && !response.dragged()) {
                        if let Some(tx) = &self.playback {
                            let _ = tx.send(PlaybackCommand::Seek(position));
                        }
                    }
                    ui.label(time_label(self.now.duration));
                });
            });
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(14, 17, 18))
                    .inner_margin(20.0),
            )
            .show(ctx, |ui| {
                if self.show_settings {
                    ui.heading("Ayarlar");
                    ui.label("Spotify Client ID");
                    ui.text_edit_singleline(&mut self.client_id);
                    if ui
                        .add_enabled(
                            self.pending.is_none(),
                            egui::Button::new("Spotify ile giris yap"),
                        )
                        .clicked()
                    {
                        self.start(ctx, "login");
                    }
                    ui.add_space(10.0);
                    ui.label("Soloist API anahtari");
                    ui.add(egui::TextEdit::singleline(&mut self.soloist_key).password(true));
                    ui.label("Soloist dosya yolu (istege bagli)");
                    ui.text_edit_singleline(&mut self.soloist_path);
                    if ui.button("Oynaticiyi baslat").clicked() {
                        self.start_soloist(ctx);
                    }
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    if ui.button("Ara").clicked() {
                        self.section = "search".into();
                        self.results.clear();
                    }
                    if ui.button("Begendiklerim").clicked() {
                        self.load_library(ctx, "liked".into(), false);
                    }
                    if ui.button("Calma listelerim").clicked() {
                        self.load_library(ctx, "playlists".into(), false);
                    }
                });
                ui.label(RichText::new("Ne dinlemek istersin?").size(25.0).strong());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("Sarki veya sanatci ara")
                            .desired_width((ui.available_width() - 80.0).max(80.0)),
                    );
                    if ui
                        .add_enabled(self.pending.is_none(), egui::Button::new("Ara"))
                        .clicked()
                        || (self.pending.is_none()
                            && response.lost_focus()
                            && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        self.section = "search".into();
                        self.append_page = false;
                        self.start(ctx, "search");
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(&self.status)
                        .color(Color32::from_gray(150))
                        .size(12.0),
                );
                ui.add_space(12.0);
                if !self.results.is_empty() {
                    ui.label(
                        RichText::new("SONUCLAR")
                            .strong()
                            .color(Color32::from_gray(150)),
                    );
                }
                let mut selected = None;
                egui::ScrollArea::vertical()
                    .max_height((ui.available_height() - 42.0).max(80.0))
                    .show(ui, |ui| {
                        for track in self.results.clone() {
                            let cover = track.image.as_deref().and_then(|url| self.cover(ctx, url));
                            ui.horizontal(|ui| {
                                if let Some(texture) = cover {
                                    ui.add(
                                        egui::Image::new(&texture)
                                            .fit_to_exact_size(Vec2::splat(40.0)),
                                    );
                                } else {
                                    ui.allocate_space(Vec2::splat(40.0));
                                }
                                if ui
                                    .add_sized(
                                        [(ui.available_width() - 85.0).max(40.0), 40.0],
                                        egui::Button::new(RichText::new(&track.label).size(15.0))
                                            .fill(Color32::from_rgb(27, 31, 32))
                                            .stroke(Stroke::NONE),
                                    )
                                    .clicked()
                                {
                                    selected = Some(track.clone());
                                }
                                if let Some(url) = &track.url {
                                    if ui.small_button("Spotify").clicked() {
                                        let _ = open::that(url);
                                    }
                                }
                            });
                        }
                    });
                if self.has_more
                    && self.section != "search"
                    && ui
                        .add_enabled(self.pending.is_none(), egui::Button::new("Daha fazla"))
                        .clicked()
                {
                    self.load_library(ctx, self.section.clone(), true);
                }
                if let Some(track) = selected {
                    if track.playlist {
                        self.load_library(ctx, track.id, false);
                    } else {
                        self.play_track(ctx, track);
                    }
                }
            });
    }
}
fn desktop() -> Result<()> {
    eframe::run_native(
        "Spotimini",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_inner_size([850.0, 620.0])
                .with_min_inner_size([480.0, 440.0]),
            ..Default::default()
        },
        Box::new(|_| Ok(Box::new(DesktopApp::default()))),
    )
    .map_err(|e| e.to_string().into())
}

fn run() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() == 1 {
        return desktop();
    }
    let client_id = env::var("SPOTIMINI_CLIENT_ID")
        .map_err(|_| "SPOTIMINI_CLIENT_ID ortam degiskenini ayarlayin")?;
    match args.get(1).map(String::as_str) {
        Some("login") => login(&client_id),
        Some("search") if args.len() > 2 => {
            for track in search_tracks(&client_id, &args[2..].join(" "))? {
                println!("{}", track.label);
            }
            Ok(())
        }
        Some("logout") => {
            credential(&client_id)?.delete_credential()?;
            println!("Oturum silindi.");
            Ok(())
        }
        _ => Err("Kullanim: spotimini login | search <sorgu> | logout".into()),
    }
}
fn main() {
    let mut logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn"));
    logger.filter_module("symphonia_bundle_mp3::demuxer", log::LevelFilter::Error);
    logger.init();
    if let Err(e) = run() {
        eprintln!("Hata: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closing_app_stops_the_soloist_process() {
        use std::os::windows::process::CommandExt;
        let child = std::process::Command::new("ping.exe")
            .args(["-n", "30", "127.0.0.1"])
            .creation_flags(0x08000000)
            .spawn()
            .unwrap();
        let mut process = SoloistProcess {
            child: Some(child),
            closed: false,
        };
        process.stop();
        assert!(process.closed);
        assert!(process
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_some());
    }
    #[test]
    fn soloist_state_is_mapped_to_player_controls() {
        let state = serde_json::json!({"status":"playing", "volume": 37, "position":{"position_ms": 65000}, "item":{"decorations":{"identity":{"name":"Track"},"creators":[{"entity":{"decorations":{"identity":{"name":"Artist"}}}}],"playback":{"duration_ms": 180000}}}});
        let now = parse_playback(&state);
        assert_eq!(
            (now.title.as_str(), now.artist.as_str()),
            ("Track", "Artist")
        );
        assert!(now.playing);
        assert_eq!(
            (now.position, now.duration, now.volume),
            (65000, 180000, 37)
        );
        assert_eq!(time_label(now.position), "1:05");
    }
    #[test]
    fn pkce_rfc_example() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
    #[test]
    fn random_verifier_is_url_safe_and_long_enough() {
        let s = random_string(64).unwrap();
        assert!((43..=128).contains(&s.len()));
        assert!(s
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
    }
}
