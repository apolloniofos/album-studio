#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ExtendedColorType, ImageDecoder, ImageReader};
use image::{imageops::FilterType, metadata::Orientation};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

const EXT: [&str; 8] = ["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];

const PAGE: &str = r####"<!DOCTYPE html><html lang="de"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>@TITLE@</title>
<style>
body{margin:0;background:@BG@;color:@FG@;font-family:Segoe UI,Arial,sans-serif}
h1{text-align:center;font-weight:300;padding:28px 12px 8px;margin:0}
p.n{text-align:center;opacity:.6;margin:0 0 18px}
.g{display:grid;grid-template-columns:repeat(auto-fill,minmax(@T@px,1fr));gap:10px;padding:14px;max-width:1400px;margin:auto}
.g a{background:@CARD@;border-radius:6px;overflow:hidden;display:block;cursor:pointer}
.g img{width:100%;aspect-ratio:1;object-fit:cover;display:block;transition:transform .2s}
.g a:hover img{transform:scale(1.05)}
#lb{display:none;position:fixed;inset:0;background:rgba(0,0,0,.94);z-index:9;flex-direction:column;align-items:center;justify-content:center}
#lb.on{display:flex}#lb img{max-width:94vw;max-height:84vh}
#lb p{color:#ddd;margin:12px}#lb button{position:absolute;background:none;border:0;color:#fff;font-size:42px;cursor:pointer;padding:16px}
#x{top:0;right:8px}#pv{left:6px;top:45%}#nx{right:6px;top:45%}
</style></head><body><h1>@TITLE@</h1><p class="n">@COUNT@ Bilder</p><div class="g" id="g"></div>
<div id="lb"><button id="x">&times;</button><button id="pv">&#8249;</button><button id="nx">&#8250;</button>
<img id="im" alt=""><p id="cp"></p></div>
<script>
const D=@DATA@;let i=0;const $=id=>document.getElementById(id);
D.forEach((d,k)=>{const a=document.createElement('a');a.innerHTML='<img loading="lazy" src="'+d.t+'" alt="">';
a.onclick=()=>show(k);$('g').appendChild(a)});
function show(k){i=(k+D.length)%D.length;$('im').src=D[i].s;$('cp').textContent=D[i].c;$('lb').classList.add('on')}
$('x').onclick=()=>$('lb').classList.remove('on');$('pv').onclick=()=>show(i-1);$('nx').onclick=()=>show(i+1);
document.onkeydown=e=>{if(!$('lb').classList.contains('on'))return;
if(e.key=='ArrowLeft')show(i-1);if(e.key=='ArrowRight')show(i+1);if(e.key=='Escape')$('x').click()};
</script></body></html>"####;

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Config {
    src: String,
    dst: String,
    title: String,
    thumb: u32,
    size: u32,
    quality: u8,
    theme: String,
    originals: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { src: String::new(), dst: String::new(), title: "Mein Album".into(),
                 thumb: 240, size: 1600, quality: 85, theme: "dunkel".into(), originals: false }
    }
}

#[derive(Serialize)]
struct Item { t: String, s: String, c: String }

fn cfg_path() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("AlbumStudio.json")))
        .unwrap_or_else(|| PathBuf::from("AlbumStudio.json"))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn save_jpg(img: &DynamicImage, path: &Path, q: u8) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = img.to_rgb8();
    let mut enc = JpegEncoder::new_with_quality(BufWriter::new(File::create(path)?), q);
    enc.encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)?;
    Ok(())
}

fn process(i: usize, p: &Path, cfg: &Config, dst: &Path) -> Result<Item, String> {
    (|| -> Result<Item, Box<dyn std::error::Error>> {
        let mut dec = ImageReader::open(p)?.with_guessed_format()?.into_decoder()?;
        let orient = dec.orientation().unwrap_or(Orientation::NoTransforms);
        let mut img = DynamicImage::from_decoder(dec)?;
        img.apply_orientation(orient);
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let safe: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let name = format!("{:04}_{}.jpg", i + 1, safe);
        let slide = if img.width() > cfg.size || img.height() > cfg.size {
            img.resize(cfg.size, cfg.size, FilterType::Lanczos3)
        } else { img.clone() };
        save_jpg(&slide, &dst.join("slides").join(&name), cfg.quality)?;
        let th = img.resize_to_fill(cfg.thumb * 2, cfg.thumb * 2, FilterType::Triangle);
        save_jpg(&th, &dst.join("thumbs").join(&name), 80)?;
        if cfg.originals {
            fs::copy(p, dst.join("originals").join(p.file_name().unwrap()))?;
        }
        Ok(Item { t: format!("thumbs/{name}"), s: format!("slides/{name}"),
                  c: stem.replace(['_', '-'], " ") })
    })().map_err(|e| e.to_string())
}

fn generate(cfg: &Config, done: &AtomicUsize, total: &AtomicUsize, ctx: &egui::Context) -> Result<PathBuf, String> {
    let dst = PathBuf::from(&cfg.dst);
    let mut files: Vec<PathBuf> = fs::read_dir(&cfg.src).map_err(|e| e.to_string())?
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str())
            .map(|x| EXT.contains(&x.to_lowercase().as_str())).unwrap_or(false))
        .collect();
    if files.is_empty() { return Err("Im Quellordner wurden keine Bilder gefunden.".into()); }
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let mut dirs = vec!["thumbs", "slides"];
    if cfg.originals { dirs.push("originals"); }
    for d in dirs { fs::create_dir_all(dst.join(d)).map_err(|e| e.to_string())?; }
    total.store(files.len(), Relaxed);
    done.store(0, Relaxed);
    let items: Vec<Item> = files.par_iter().enumerate().filter_map(|(i, p)| {
        let r = process(i, p, cfg, &dst);
        done.fetch_add(1, Relaxed);
        ctx.request_repaint();
        r.ok()
    }).collect();
    if items.is_empty() { return Err("Kein Bild konnte verarbeitet werden.".into()); }
    let (bg, fg, card) = if cfg.theme == "hell" { ("#f6f6f6", "#222", "#fff") } else { ("#111", "#eee", "#1c1c1c") };
    let data = serde_json::to_string(&items).map_err(|e| e.to_string())?.replace("</", "<\\/");
    let html = PAGE.replace("@TITLE@", &esc(&cfg.title)).replace("@BG@", bg).replace("@FG@", fg)
        .replace("@CARD@", card).replace("@T@", &cfg.thumb.to_string())
        .replace("@COUNT@", &items.len().to_string()).replace("@DATA@", &data);
    let out = dst.join("index.html");
    fs::write(&out, html).map_err(|e| e.to_string())?;
    Ok(out)
}

struct App {
    cfg: Config,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    running: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<PathBuf, String>>>>,
    status: String,
}

impl App {
    fn new() -> Self {
        let cfg = fs::read_to_string(cfg_path()).ok()
            .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        App { cfg, done: Default::default(), total: Default::default(), running: Default::default(),
              result: Default::default(), status: "Bereit.".into() }
    }

    fn start(&mut self, ctx: &egui::Context) {
        if !Path::new(&self.cfg.src).is_dir() || self.cfg.dst.is_empty() {
            self.status = "Bitte Foto- und Zielordner wählen.".into();
            return;
        }
        if let Ok(s) = serde_json::to_string_pretty(&self.cfg) { let _ = fs::write(cfg_path(), s); }
        self.status = "Erstelle Galerie …".into();
        self.running.store(true, Relaxed);
        let (cfg, done, total, running, result, ctx) = (self.cfg.clone(), self.done.clone(),
            self.total.clone(), self.running.clone(), self.result.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = generate(&cfg, &done, &total, &ctx);
            *result.lock().unwrap() = Some(r);
            running.store(false, Relaxed);
            ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(r) = self.result.lock().unwrap().take() {
            match r {
                Ok(p) => { self.status = "Fertig! Galerie wird geöffnet.".into(); let _ = open::that(&p); }
                Err(e) => self.status = format!("Fehler: {e}"),
            }
        }
        let running = self.running.load(Relaxed);
        let mut go = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Album Studio");
            ui.add_space(8.0);
            egui::Grid::new("g").num_columns(3).spacing([8.0, 8.0]).show(ui, |ui| {
                ui.label("Fotoordner");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.src).desired_width(320.0));
                if ui.button("…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() {
                        self.cfg.src = p.display().to_string();
                        if self.cfg.dst.is_empty() { self.cfg.dst = p.join("_album").display().to_string(); }
                    }
                }
                ui.end_row();
                ui.label("Zielordner");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.dst).desired_width(320.0));
                if ui.button("…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().pick_folder() { self.cfg.dst = p.display().to_string(); }
                }
                ui.end_row();
                ui.label("Albumtitel");
                ui.add(egui::TextEdit::singleline(&mut self.cfg.title).desired_width(320.0));
                ui.end_row();
                ui.label("Vorschaugröße");
                ui.add(egui::Slider::new(&mut self.cfg.thumb, 100..=500).suffix(" px"));
                ui.end_row();
                ui.label("Bildgröße max.");
                ui.add(egui::Slider::new(&mut self.cfg.size, 600..=4000).suffix(" px"));
                ui.end_row();
                ui.label("JPEG-Qualität");
                ui.add(egui::Slider::new(&mut self.cfg.quality, 40..=100));
                ui.end_row();
                ui.label("Design");
                egui::ComboBox::from_id_salt("theme").selected_text(self.cfg.theme.clone()).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.cfg.theme, "dunkel".to_string(), "dunkel");
                    ui.selectable_value(&mut self.cfg.theme, "hell".to_string(), "hell");
                });
                ui.end_row();
            });
            ui.checkbox(&mut self.cfg.originals, "Originale mitkopieren (Download)");
            ui.add_space(12.0);
            let (d, t) = (self.done.load(Relaxed), self.total.load(Relaxed));
            let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).show_percentage());
            if running { ui.label(format!("{d} / {t} Bilder")); } else { ui.label(&self.status); }
            ui.add_space(8.0);
            if ui.add_enabled(!running, egui::Button::new("Galerie erstellen")).clicked() { go = true; }
        });
        if go { self.start(ctx); }
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([560.0, 470.0]).with_resizable(false),
        ..Default::default()
    };
    eframe::run_native("Album Studio", opts, Box::new(|_cc| Ok(Box::new(App::new()))))
}
