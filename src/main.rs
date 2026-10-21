#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::{self, Color32, RichText};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, metadata::Orientation};
use image::{DynamicImage, ExtendedColorType, ImageDecoder, ImageReader};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

const NAME: &str = "Apollonios AlbumStudio";
const BG: &[u8] = include_bytes!("../bg.jpg");
const EXT: [&str; 8] = ["jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff"];

const PAGE: &str = r####"<!DOCTYPE html><html lang="de"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><meta name="keywords" content="@TAGS@"><title>@TITLE@</title>
<style>
body{margin:0;background:@BG@;color:@FG@;font-family:Segoe UI,Arial,sans-serif}
h1{text-align:center;font-weight:300;padding:28px 12px 8px;margin:0}
p.n{text-align:center;opacity:.6;margin:0 0 18px}
@GRID@
.g a{background:@CARD@;border-radius:6px;overflow:hidden;display:block;cursor:pointer}
footer{text-align:center;opacity:.4;font-size:12px;padding:24px}
#lb{display:none;position:fixed;inset:0;background:rgba(0,0,0,.94);z-index:9;flex-direction:column;align-items:center;justify-content:center}
#lb.on{display:flex}#lb img{max-width:94vw;max-height:84vh}
#lb p{color:#ddd;margin:12px}#lb button{position:absolute;background:none;border:0;color:#fff;font-size:42px;cursor:pointer;padding:16px}
#x{top:0;right:8px}#pv{left:6px;top:45%}#nx{right:6px;top:45%}
@EXTRA@
</style></head><body><h1>@TITLE@</h1><p class="n">@DESC@@COUNT@ Bilder</p><div class="g" id="g"></div>
<footer>Erstellt mit Apollonios AlbumStudio</footer>
<div id="lb"><button id="x">&times;</button><button id="pv">&#8249;</button><button id="nx">&#8250;</button>
<img id="im" alt=""><p id="cp"></p></div>
<script>
const D=@DATA@;let i=0;const $=id=>document.getElementById(id);
D.forEach((d,k)=>{const a=document.createElement('a');a.innerHTML='<img loading="lazy" src="'+d.t+'" alt="">';
a.style.setProperty('--r',d.r);a.style.setProperty('--g',d.r*100);a.onclick=()=>show(k);$('g').appendChild(a)});
function show(k){i=(k+D.length)%D.length;$('im').src=D[i].s;$('cp').textContent=D[i].c+(D[i].d?' - '+D[i].d:'');$('lb').classList.add('on')}
$('x').onclick=()=>$('lb').classList.remove('on');$('pv').onclick=()=>show(i-1);$('nx').onclick=()=>show(i+1);
document.onkeydown=e=>{if(!$('lb').classList.contains('on'))return;
if(e.key=='ArrowLeft')show(i-1);if(e.key=='ArrowRight')show(i+1);if(e.key=='Escape')$('x').click()};
</script></body></html>"####;

#[derive(Serialize, Deserialize, Clone, Default)]
#[serde(default)]
struct Entry { path: String, title: String, comment: String, excluded: bool }

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Project {
    name: String, desc: String, tags: String, skin: String, style: String,
    thumb: u32, size: u32, quality: u8, originals: bool, out: String,
    items: Vec<Entry>,
    #[serde(skip)] path: Option<PathBuf>,
    #[serde(skip)] dirty: bool,
}

impl Default for Project {
    fn default() -> Self {
        Project { name: "Leeres Projekt".into(), desc: String::new(), tags: String::new(),
            skin: "Plain".into(), style: "White".into(), thumb: 240, size: 1600, quality: 85,
            originals: false, out: String::new(), items: vec![], path: None, dirty: false }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Cfg { recent: Vec<String>, design: String }

#[derive(PartialEq, Clone, Copy)]
enum Tab { Explorer, Edit, Check, Share }
#[derive(PartialEq, Clone, Copy)]
enum Modal { None, New, Settings, About }

enum Act {
    NewDlg, OpenDlg, Open(PathBuf), Save, SaveAs, Close, Quit, Add, AddFolder, Exclude, Remove,
    SelectAll, Invert, Sort(u8), Refresh, Build, Preview, Settings, About, OpenOut, OpenBrowser, Tab(Tab),
}

fn docs() -> PathBuf {
    let h = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).unwrap_or_else(|_| ".".into());
    PathBuf::from(h).join("Documents").join("Apollonios AlbumStudio")
}
fn cfg_path() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("AlbumStudio.json")))
        .unwrap_or_else(|| PathBuf::from("AlbumStudio.json"))
}
fn stem_of(s: &str) -> String {
    Path::new(s).file_stem().map(|x| x.to_string_lossy().to_string()).unwrap_or_default()
}
fn is_img(p: &Path) -> bool {
    p.extension().and_then(|x| x.to_str()).map(|x| EXT.contains(&x.to_lowercase().as_str())).unwrap_or(false)
}
fn collect(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        if let Ok(rd) = fs::read_dir(p) {
            let mut v: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            v.sort();
            for q in v { collect(&q, out); }
        }
    } else if is_img(p) { out.push(p.to_path_buf()); }
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
fn shorten(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}...", s.chars().take(n.saturating_sub(3)).collect::<String>()) }
}
fn load(p: &Path) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let mut dec = ImageReader::open(p)?.with_guessed_format()?.into_decoder()?;
    let o = dec.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(o);
    Ok(img)
}
fn thumb(p: &Path) -> Option<egui::ColorImage> {
    let t = load(p).ok()?.thumbnail(256, 256).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw()))
}
fn save_jpg(img: &DynamicImage, path: &Path, q: u8) -> Result<(), Box<dyn std::error::Error>> {
    let rgb = img.to_rgb8();
    let mut enc = JpegEncoder::new_with_quality(BufWriter::new(File::create(path)?), q);
    enc.encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)?;
    Ok(())
}

#[derive(Serialize)]
struct Item { t: String, s: String, c: String, d: String, r: f32 }

fn build(pr: &Project, out: &Path, done: &AtomicUsize, total: &AtomicUsize, ctx: &egui::Context) -> Result<PathBuf, String> {
    let list: Vec<(usize, &Entry)> = pr.items.iter().filter(|e| !e.excluded).enumerate().collect();
    if list.is_empty() { return Err("Das Projekt enthält keine Bilder.".into()); }
    let mut dirs = vec!["thumbs", "slides"];
    if pr.originals { dirs.push("originals"); }
    for d in dirs { fs::create_dir_all(out.join(d)).map_err(|e| e.to_string())?; }
    total.store(list.len(), Relaxed);
    done.store(0, Relaxed);
    let mosaic = matches!(pr.skin.as_str(), "Zeilen" | "Mosaik");
    let items: Vec<Item> = list.par_iter().filter_map(|(i, e)| {
        let r = (|| -> Result<Item, Box<dyn std::error::Error>> {
            let p = Path::new(&e.path);
            let img = load(p)?;
            let safe: String = stem_of(&e.path).chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
            let name = format!("{:04}_{}.jpg", i + 1, safe);
            let slide = if img.width() > pr.size || img.height() > pr.size {
                img.resize(pr.size, pr.size, FilterType::Lanczos3) } else { img.clone() };
            save_jpg(&slide, &out.join("slides").join(&name), pr.quality)?;
            let th = if mosaic { img.resize(pr.thumb * 2, pr.thumb * 2, FilterType::Triangle) }
                     else { img.resize_to_fill(pr.thumb * 2, pr.thumb * 2, FilterType::Triangle) };
            save_jpg(&th, &out.join("thumbs").join(&name), 80)?;
            if pr.originals { fs::copy(p, out.join("originals").join(p.file_name().unwrap()))?; }
            Ok(Item { t: format!("thumbs/{name}"), s: format!("slides/{name}"), c: e.title.clone(), d: e.comment.clone(), r: th.width() as f32 / th.height() as f32 })
        })();
        done.fetch_add(1, Relaxed);
        ctx.request_repaint();
        r.ok()
    }).collect();
    if items.is_empty() { return Err("Kein Bild konnte verarbeitet werden.".into()); }
    let (bg, fg, card) = match pr.style.as_str() {
        "Altgriechisch" => ("#1b1410", "#f3e9d2", "rgba(40,28,18,.8)"), "Dark" => ("#111", "#eee", "#1c1c1c"), "Sand" => ("#efe6d6", "#3a2f20", "#f8f1e4"), _ => ("#fff", "#222", "#f2f2f2") };
    let t = pr.thumb;
    let grid = if mosaic {
        format!(".g{{display:flex;flex-wrap:wrap;gap:10px;padding:14px;max-width:1400px;margin:auto}}.g::after{{content:\"\";flex-grow:1000000}}.g a{{height:{t}px;flex-grow:var(--g);flex-basis:calc({t}px*var(--r))}}.g img{{width:100%;height:100%;object-fit:cover;display:block}}")
    } else {
        format!(".g{{display:grid;grid-template-columns:repeat(auto-fill,minmax({t}px,1fr));gap:10px;padding:14px;max-width:1400px;margin:auto}}.g img{{width:100%;aspect-ratio:1;object-fit:cover;display:block}}")
    };
    let data = serde_json::to_string(&items).map_err(|e| e.to_string())?.replace("</", "<\\/");
    let desc = if pr.desc.is_empty() { String::new() } else { format!("{} - ", esc(&pr.desc)) };
    let greek = pr.style == "Altgriechisch";
    let extra = if greek { "body{background:linear-gradient(rgba(20,12,6,.6),rgba(20,12,6,.75)),url(bg.jpg) center/cover fixed;font-family:Georgia,'Palatino Linotype',serif}h1{color:#e0b85a;letter-spacing:3px;font-weight:400;text-shadow:0 2px 8px #000;border-bottom:2px solid #c9a24b;max-width:520px;margin:0 auto}.g a{border:2px solid #c9a24b}" } else { "" };
    if greek { fs::write(out.join("bg.jpg"), BG).map_err(|e| e.to_string())?; }
    let html = PAGE.replace("@GRID@", &grid).replace("@EXTRA@", extra).replace("@TITLE@", &esc(&pr.name)).replace("@TAGS@", &esc(&pr.tags))
        .replace("@BG@", bg).replace("@FG@", fg).replace("@CARD@", card).replace("@DESC@", &desc)
        .replace("@COUNT@", &items.len().to_string()).replace("@DATA@", &data);
    let index = out.join("index.html");
    fs::write(&index, html).map_err(|e| e.to_string())?;
    Ok(index)
}

fn mi(ui: &mut egui::Ui, acts: &mut Vec<Act>, label: &str, sc: &str, a: Act) {
    let mut b = egui::Button::new(label);
    if !sc.is_empty() { b = b.shortcut_text(sc); }
    if ui.add(b).clicked() { acts.push(a); ui.close_menu(); }
}

struct App {
    proj: Project, cfg: Cfg, tab: Tab, modal: Modal, sel: HashSet<usize>, zoom: f32, hide: bool,
    tex: HashMap<PathBuf, egui::TextureHandle>, tx: Sender<(PathBuf, Option<egui::ColorImage>)>,
    rx: Receiver<(PathBuf, Option<egui::ColorImage>)>,
    done: Arc<AtomicUsize>, total: Arc<AtomicUsize>, running: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<PathBuf, String>>>>,
    status: String, new_name: String, new_desc: String, new_tags: String, last_title: String,
    bg: Option<egui::TextureHandle>,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        let cfg: Cfg = fs::read_to_string(cfg_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let (tx, rx) = channel();
        let bg = image::load_from_memory(BG).ok().map(|i| {
            let t = i.to_rgba8();
            ctx.load_texture("bg", egui::ColorImage::from_rgba_unmultiplied([t.width() as usize, t.height() as usize], t.as_raw()), egui::TextureOptions::LINEAR)
        });
        theme(ctx, cfg.design != "Dunkel");
        App { proj: Project::default(), cfg, tab: Tab::Explorer, modal: Modal::None, sel: HashSet::new(),
            zoom: 150.0, hide: false, tex: HashMap::new(), tx, rx, done: Default::default(), total: Default::default(),
            running: Default::default(), result: Default::default(),
            status: "Wählen Sie einen Bilderordner oder nutzen Sie Drag & Drop und klicken Sie dann \"Album erstellen\".".into(),
            new_name: String::new(), new_desc: String::new(), new_tags: String::new(), last_title: String::new(), bg }
    }

    fn save_cfg(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.cfg) { let _ = fs::write(cfg_path(), s); }
    }
    fn add_recent(&mut self, p: &Path) {
        let s = p.to_string_lossy().to_string();
        self.cfg.recent.retain(|x| x != &s);
        self.cfg.recent.insert(0, s);
        self.cfg.recent.truncate(8);
        self.save_cfg();
    }
    fn load_thumbs(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        rayon::spawn(move || {
            paths.par_iter().for_each_with(tx, |tx, p| {
                let _ = tx.send((p.clone(), thumb(p)));
                ctx.request_repaint();
            });
        });
    }
    fn add_paths(&mut self, paths: Vec<PathBuf>, ctx: &egui::Context) {
        let mut all = vec![];
        for p in &paths { collect(p, &mut all); }
        let have: HashSet<String> = self.proj.items.iter().map(|e| e.path.clone()).collect();
        let new: Vec<PathBuf> = all.into_iter().filter(|p| !have.contains(&p.to_string_lossy().to_string())).collect();
        if new.is_empty() { return; }
        for p in &new {
            let s = p.to_string_lossy().to_string();
            self.proj.items.push(Entry { title: stem_of(&s).replace(['_', '-'], " "), path: s, comment: String::new(), excluded: false });
        }
        self.proj.dirty = true;
        self.status = format!("{} Bilder hinzugefügt.", new.len());
        self.load_thumbs(new, ctx);
    }
    fn reload_all(&mut self, ctx: &egui::Context) {
        self.tex.clear();
        let v: Vec<PathBuf> = self.proj.items.iter().map(|e| PathBuf::from(&e.path)).collect();
        self.load_thumbs(v, ctx);
    }
    fn open_project(&mut self, p: PathBuf, ctx: &egui::Context) {
        match fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| serde_json::from_str::<Project>(&s).map_err(|e| e.to_string())) {
            Ok(mut pr) => {
                pr.path = Some(p.clone());
                self.proj = pr;
                self.sel.clear();
                self.reload_all(ctx);
                self.add_recent(&p);
                self.status = format!("Projekt geöffnet: {}", p.display());
            }
            Err(e) => self.status = format!("Projekt konnte nicht geöffnet werden: {e}"),
        }
    }
    fn save(&mut self, as_new: bool) {
        let path = match (&self.proj.path, as_new) {
            (Some(p), false) => p.clone(),
            _ => {
                let _ = fs::create_dir_all(docs());
                match rfd::FileDialog::new().set_directory(docs()).set_file_name(format!("{}.aas", self.proj.name))
                    .add_filter("AlbumStudio-Projekt", &["aas"]).save_file() {
                    Some(p) => p, None => return,
                }
            }
        };
        match serde_json::to_string_pretty(&self.proj).map_err(|e| e.to_string()).and_then(|s| fs::write(&path, s).map_err(|e| e.to_string())) {
            Ok(_) => { self.proj.path = Some(path.clone()); self.proj.dirty = false; self.add_recent(&path); self.status = "Projekt gespeichert.".into(); }
            Err(e) => self.status = format!("Speichern fehlgeschlagen: {e}"),
        }
    }
    fn out_dir(&self) -> PathBuf {
        if self.proj.out.is_empty() {
            let safe: String = self.proj.name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
            docs().join("Web Albums").join(safe)
        } else { PathBuf::from(&self.proj.out) }
    }
    fn start_build(&mut self, preview: bool, ctx: &egui::Context) {
        if self.running.load(Relaxed) { return; }
        let out = if preview {
            let d = std::env::temp_dir().join("ApolloniosAlbumStudio_Preview");
            let _ = fs::remove_dir_all(&d);
            d
        } else { self.out_dir() };
        if !preview && self.proj.out.is_empty() { self.proj.out = out.to_string_lossy().to_string(); self.proj.dirty = true; }
        self.status = if preview { "Erstelle Vorschau ...".into() } else { "Erstelle Album ...".into() };
        self.running.store(true, Relaxed);
        let (pr, done, total, running, result, ctx) = (self.proj.clone(), self.done.clone(), self.total.clone(),
            self.running.clone(), self.result.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = build(&pr, &out, &done, &total, &ctx);
            *result.lock().unwrap() = Some(r);
            running.store(false, Relaxed);
            ctx.request_repaint();
        });
    }
    fn run(&mut self, a: Act, ctx: &egui::Context) {
        match a {
            Act::NewDlg => { self.new_name.clear(); self.new_desc.clear(); self.new_tags.clear(); self.modal = Modal::New; }
            Act::OpenDlg => {
                let _ = fs::create_dir_all(docs());
                if let Some(p) = rfd::FileDialog::new().set_directory(docs()).add_filter("AlbumStudio-Projekt", &["aas"]).pick_file() { self.open_project(p, ctx); }
            }
            Act::Open(p) => self.open_project(p, ctx),
            Act::Save => self.save(false),
            Act::SaveAs => self.save(true),
            Act::Close => { self.proj = Project::default(); self.sel.clear(); self.tex.clear(); }
            Act::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Act::Add => {
                if let Some(v) = rfd::FileDialog::new().add_filter("Bilder", &EXT).pick_files() { self.add_paths(v, ctx); }
            }
            Act::AddFolder => { if let Some(p) = rfd::FileDialog::new().pick_folder() { self.add_paths(vec![p], ctx); } }
            Act::Exclude => {
                for &i in &self.sel { if let Some(e) = self.proj.items.get_mut(i) { e.excluded = !e.excluded; } }
                self.proj.dirty = true;
            }
            Act::Remove => {
                let mut v: Vec<usize> = self.sel.iter().cloned().collect();
                v.sort_unstable_by(|a, b| b.cmp(a));
                for i in v { if i < self.proj.items.len() { self.proj.items.remove(i); } }
                self.sel.clear();
                self.proj.dirty = true;
            }
            Act::SelectAll => self.sel = (0..self.proj.items.len()).collect(),
            Act::Invert => { let n = self.proj.items.len(); self.sel = (0..n).filter(|i| !self.sel.contains(i)).collect(); }
            Act::Sort(k) => {
                match k {
                    0 => self.proj.items.sort_by_key(|e| stem_of(&e.path).to_lowercase()),
                    1 => { self.proj.items.sort_by_key(|e| stem_of(&e.path).to_lowercase()); self.proj.items.reverse(); }
                    _ => self.proj.items.sort_by_key(|e| fs::metadata(&e.path).and_then(|m| m.modified()).ok()),
                }
                self.sel.clear();
                self.proj.dirty = true;
            }
            Act::Refresh => self.reload_all(ctx),
            Act::Build => self.start_build(false, ctx),
            Act::Preview => self.start_build(true, ctx),
            Act::Settings => self.modal = Modal::Settings,
            Act::About => self.modal = Modal::About,
            Act::OpenOut => { let d = self.out_dir(); if d.exists() { let _ = open::that(d); } else { self.status = "Der Ausgabeordner existiert noch nicht.".into(); } }
            Act::OpenBrowser => { let i = self.out_dir().join("index.html"); if i.exists() { let _ = open::that(i); } else { self.status = "Es wurde noch kein Album erstellt.".into(); } }
            Act::Tab(t) => self.tab = t,
        }
    }
}

fn theme(ctx: &egui::Context, greek: bool) {
    let mut v = egui::Visuals::dark();
    if greek {
        let brown = Color32::from_rgb(78, 54, 32);
        v.panel_fill = Color32::from_rgba_unmultiplied(24, 16, 10, 210);
        v.window_fill = Color32::from_rgb(38, 27, 18);
        v.extreme_bg_color = Color32::from_rgb(22, 15, 9);
        v.selection.bg_fill = Color32::from_rgb(156, 72, 42);
        v.widgets.inactive.bg_fill = brown;
        v.widgets.inactive.weak_bg_fill = brown;
        v.widgets.hovered.bg_fill = Color32::from_rgb(120, 84, 44);
        v.widgets.hovered.weak_bg_fill = Color32::from_rgb(120, 84, 44);
    } else {
        v.panel_fill = Color32::from_rgb(26, 26, 26);
        v.window_fill = Color32::from_rgb(34, 34, 34);
        v.extreme_bg_color = Color32::from_rgb(20, 20, 20);
        v.selection.bg_fill = Color32::from_rgb(74, 125, 232);
    }
    ctx.set_visuals(v);
}

const GOLD: Color32 = Color32::from_rgb(214, 175, 85);

/// Mäander (griechisches Schlüsselmuster) als Zierleiste
fn meander(p: &egui::Painter, r: egui::Rect) {
    let st = egui::Stroke::new(1.6, GOLD);
    let h = r.height().min(14.0);
    let y = r.min.y;
    let mut x = r.min.x;
    p.line_segment([egui::pos2(r.min.x, y + h), egui::pos2(r.max.x, y + h)], st);
    while x + h <= r.max.x {
        let pts = vec![egui::pos2(x, y + h), egui::pos2(x, y), egui::pos2(x + h, y), egui::pos2(x + h, y + h * 0.7),
            egui::pos2(x + h * 0.3, y + h * 0.7), egui::pos2(x + h * 0.3, y + h * 0.35), egui::pos2(x + h * 0.65, y + h * 0.35)];
        let _ = p.add(egui::Shape::line(pts, st));
        x += h + 3.0;
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        let greek = self.cfg.design != "Dunkel";
        if greek {
            if let Some(t) = &self.bg {
                let sr = ctx.screen_rect();
                let (iw, ih) = (t.size_vec2().x, t.size_vec2().y);
                let (ia, sa) = (iw / ih, sr.width() / sr.height());
                let (u0, u1, v0, v1) = if sa > ia { let h = ia / sa; let o = (1.0 - h) * 0.6; (0.0, 1.0, o, o + h) }
                    else { let w = sa / ia; let o = (1.0 - w) / 2.0; (o, o + w, 0.0, 1.0) };
                ctx.layer_painter(egui::LayerId::background()).image(t.id(), sr,
                    egui::Rect::from_min_max(egui::pos2(u0, v0), egui::pos2(u1, v1)), Color32::WHITE);
            }
        }
        while let Ok((p, ci)) = self.rx.try_recv() {
            if let Some(ci) = ci { let h = ctx.load_texture(p.to_string_lossy(), ci, egui::TextureOptions::LINEAR); self.tex.insert(p, h); }
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() { self.add_paths(dropped, ctx); }
        let res = self.result.lock().unwrap().take();
        if let Some(r) = res {
            match r {
                Ok(p) => { self.status = format!("Album erstellt: {}", p.display()); let _ = open::that(&p); }
                Err(e) => self.status = format!("Fehler: {e}"),
            }
        }
        let running = self.running.load(Relaxed);
        let mut acts: Vec<Act> = vec![];
        ctx.input(|i| {
            if i.key_pressed(egui::Key::F9) { acts.push(Act::Build); }
            if i.key_pressed(egui::Key::F12) { acts.push(Act::Preview); }
            if i.key_pressed(egui::Key::F4) { acts.push(Act::Settings); }
            if i.key_pressed(egui::Key::F5) { acts.push(Act::Refresh); }
            if i.modifiers.command {
                if i.key_pressed(egui::Key::N) { acts.push(Act::NewDlg); }
                if i.key_pressed(egui::Key::O) { acts.push(Act::OpenDlg); }
                if i.key_pressed(egui::Key::S) { acts.push(Act::Save); }
                if i.key_pressed(egui::Key::A) { acts.push(Act::SelectAll); }
            }
        });

        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Datei", |ui| {
                    mi(ui, &mut acts, "Neues Projekt...", "Strg+N", Act::NewDlg);
                    mi(ui, &mut acts, "Projekt öffnen...", "Strg+O", Act::OpenDlg);
                    ui.menu_button("Letzte Projekte", |ui| {
                        for r in self.cfg.recent.clone() {
                            if ui.button(stem_of(&r)).clicked() { acts.push(Act::Open(PathBuf::from(r))); ui.close_menu(); }
                        }
                    });
                    mi(ui, &mut acts, "Projekt schließen", "", Act::Close);
                    ui.separator();
                    mi(ui, &mut acts, "Projekt speichern", "Strg+S", Act::Save);
                    mi(ui, &mut acts, "Projekt speichern unter...", "", Act::SaveAs);
                    ui.separator();
                    mi(ui, &mut acts, "Beenden", "", Act::Quit);
                });
                ui.menu_button("Bearbeiten", |ui| {
                    mi(ui, &mut acts, "Dateien hinzufügen...", "", Act::Add);
                    mi(ui, &mut acts, "Ordner hinzufügen...", "", Act::AddFolder);
                    mi(ui, &mut acts, "Ausschließen / Einschließen", "", Act::Exclude);
                    mi(ui, &mut acts, "Entfernen", "", Act::Remove);
                    ui.separator();
                    mi(ui, &mut acts, "Alles auswählen", "Strg+A", Act::SelectAll);
                    mi(ui, &mut acts, "Auswahl umkehren", "", Act::Invert);
                });
                ui.menu_button("Ansicht", |ui| {
                    ui.menu_button("Zoom", |ui| { ui.add(egui::Slider::new(&mut self.zoom, 80.0..=300.0)); });
                    ui.menu_button("Sortierung", |ui| {
                        mi(ui, &mut acts, "Name aufsteigend", "", Act::Sort(0));
                        mi(ui, &mut acts, "Name absteigend", "", Act::Sort(1));
                        mi(ui, &mut acts, "Änderungsdatum", "", Act::Sort(2));
                    });
                    ui.menu_button("Design", |ui| {
                        for d in ["Altgriechisch", "Dunkel"] {
                            let on = (d == "Dunkel") == (self.cfg.design == "Dunkel");
                            if ui.selectable_label(on, d).clicked() {
                                self.cfg.design = d.to_string();
                                theme(ctx, d != "Dunkel");
                                self.save_cfg();
                                ui.close_menu();
                            }
                        }
                    });
                    ui.checkbox(&mut self.hide, "Ausgeschlossene verbergen");
                    ui.separator();
                    mi(ui, &mut acts, "Aktualisieren", "F5", Act::Refresh);
                });
                ui.menu_button("Album", |ui| {
                    mi(ui, &mut acts, "Album erstellen", "F9", Act::Build);
                    mi(ui, &mut acts, "Vorschau", "F12", Act::Preview);
                    ui.separator();
                    mi(ui, &mut acts, "Einstellungen", "F4", Act::Settings);
                });
                ui.menu_button("Tools", |ui| {
                    mi(ui, &mut acts, "Überprüfen", "", Act::Tab(Tab::Check));
                    mi(ui, &mut acts, "Album im Browser öffnen", "", Act::OpenBrowser);
                    mi(ui, &mut acts, "Ausgabeordner öffnen", "", Act::OpenOut);
                });
                ui.menu_button("Hilfe", |ui| { mi(ui, &mut acts, "Über Apollonios AlbumStudio...", "", Act::About); });
            });
        });

        egui::SidePanel::left("left").exact_width(250.0).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Letzte Projekte").size(17.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("+").clicked() { acts.push(Act::NewDlg); }
                });
            });
            ui.separator();
            let cur = self.proj.path.as_ref().map(|p| p.to_string_lossy().to_string());
            ui.selectable_label(true, format!("{}{}", self.proj.name, if self.proj.dirty { " *" } else { "" }));
            for r in self.cfg.recent.iter() {
                if Some(r) == cur.as_ref() { continue; }
                if ui.selectable_label(false, stem_of(r)).clicked() { acts.push(Act::Open(PathBuf::from(r))); }
            }
            ui.add_space(24.0);
            ui.separator();
            ui.label(RichText::new("Skin und Style").size(17.0));
            let before = (self.proj.skin.clone(), self.proj.style.clone());
            egui::ComboBox::from_id_salt("skin").width(220.0).selected_text(self.proj.skin.clone()).show_ui(ui, |ui| {
                for s in ["Plain", "Zeilen"] { ui.selectable_value(&mut self.proj.skin, s.to_string(), s); }
            });
            egui::ComboBox::from_id_salt("style").width(220.0).selected_text(self.proj.style.clone()).show_ui(ui, |ui| {
                for s in ["White", "Dark", "Sand", "Altgriechisch"] { ui.selectable_value(&mut self.proj.style, s.to_string(), s); }
            });
            if before != (self.proj.skin.clone(), self.proj.style.clone()) { self.proj.dirty = true; }
            ui.add_space(12.0);
            let w = ui.available_width();
            if ui.add_sized([w, 30.0], egui::Button::new("Einstellungen")).clicked() { acts.push(Act::Settings); }
            if ui.add_enabled(!running, egui::Button::new("Album erstellen").min_size(egui::vec2(w, 30.0))).clicked() { acts.push(Act::Build); }
            if ui.add_enabled(!running, egui::Button::new("Vorschau").min_size(egui::vec2(w, 30.0))).clicked() { acts.push(Act::Preview); }
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if running {
                    let (d, t) = (self.done.load(Relaxed), self.total.load(Relaxed));
                    ui.add(egui::ProgressBar::new(if t > 0 { d as f32 / t as f32 } else { 0.0 }).desired_width(220.0));
                    ui.label(format!("{d} / {t} Bilder"));
                } else { ui.label(&self.status); }
            });
        });

        let cf = if greek {
            egui::Frame::default().fill(Color32::from_rgba_unmultiplied(18, 11, 7, 140)).inner_margin(egui::Margin::same(10.0))
        } else { egui::Frame::central_panel(&ctx.style()) };
        egui::CentralPanel::default().frame(cf).show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (t, n) in [(Tab::Explorer, "Explorer"), (Tab::Edit, "Bearbeiten"), (Tab::Check, "Überprüfen"), (Tab::Share, "Teilen")] {
                    ui.selectable_value(&mut self.tab, t, RichText::new(n).size(18.0));
                }
            });
            if greek {
                let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 14.0), egui::Sense::hover());
                meander(&ui.painter_at(r), r);
                ui.add_space(4.0);
            } else { ui.separator(); }
            match self.tab {
                Tab::Explorer => {
                    ui.horizontal(|ui| {
                        ui.menu_button("Hinzufügen", |ui| {
                            mi(ui, &mut acts, "Dateien...", "", Act::Add);
                            mi(ui, &mut acts, "Ordner...", "", Act::AddFolder);
                        });
                        ui.label(format!("{} Bilder, {} ausgewählt", self.proj.items.len(), self.sel.len()));
                    });
                    if self.proj.items.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() / 3.0);
                            if greek { ui.label(RichText::new("Καλώς ήρθατε").size(30.0).color(GOLD)); }
                            ui.label(RichText::new("Ziehen Sie Fotos oder Ordner\nper Drag & Drop hierher").size(22.0));
                            ui.add_space(14.0);
                            if ui.button("...oder klicken Sie: Hinzufügen").clicked() { acts.push(Act::Add); }
                        });
                    } else {
                        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                            ui.horizontal_wrapped(|ui| {
                                for i in 0..self.proj.items.len() {
                                    let (excl, path, title) = { let e = &self.proj.items[i]; (e.excluded, PathBuf::from(&e.path), e.title.clone()) };
                                    if excl && self.hide { continue; }
                                    let z = self.zoom;
                                    let (rect, resp) = ui.allocate_exact_size(egui::vec2(z, z + 22.0), egui::Sense::click());
                                    let selected = self.sel.contains(&i);
                                    let p = ui.painter_at(rect);
                                    p.rect_filled(rect, 4.0, if selected { Color32::from_rgb(58, 74, 105) } else { Color32::from_rgb(34, 34, 34) });
                                    if let Some(t) = self.tex.get(&path) {
                                        let s = t.size_vec2();
                                        let k = ((z - 8.0) / s.x).min((z - 8.0) / s.y);
                                        let r = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.min.y + z / 2.0), s * k);
                                        let tint = if excl { Color32::from_white_alpha(60) } else { Color32::WHITE };
                                        p.image(t.id(), r, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), tint);
                                    }
                                    p.text(egui::pos2(rect.center().x, rect.max.y - 11.0), egui::Align2::CENTER_CENTER,
                                        shorten(&title, (z / 8.0) as usize), egui::FontId::proportional(12.0), Color32::LIGHT_GRAY);
                                    if resp.secondary_clicked() && !self.sel.contains(&i) { self.sel.clear(); self.sel.insert(i); }
                                    if resp.clicked() {
                                        if ui.input(|i| i.modifiers.command) { if !self.sel.remove(&i) { self.sel.insert(i); } }
                                        else { self.sel.clear(); self.sel.insert(i); }
                                    }
                                    if resp.double_clicked() { acts.push(Act::Tab(Tab::Edit)); }
                                    resp.context_menu(|ui| {
                                        mi(ui, &mut acts, "Ausschließen / Einschließen", "", Act::Exclude);
                                        mi(ui, &mut acts, "Entfernen", "", Act::Remove);
                                    });
                                }
                            });
                        });
                    }
                }
                Tab::Edit => {
                    let first = self.sel.iter().min().cloned();
                    match first {
                        Some(i) if i < self.proj.items.len() => {
                            let path = PathBuf::from(&self.proj.items[i].path);
                            ui.columns(2, |c| {
                                if let Some(t) = self.tex.get(&path) {
                                    let s = t.size_vec2();
                                    let k = (c[0].available_width() / s.x).min(420.0 / s.y);
                                    c[0].add(egui::Image::new((t.id(), s * k)));
                                }
                                c[1].label("Titel");
                                if c[1].add(egui::TextEdit::singleline(&mut self.proj.items[i].title).desired_width(f32::INFINITY)).changed() { self.proj.dirty = true; }
                                c[1].label("Beschreibung");
                                if c[1].add(egui::TextEdit::multiline(&mut self.proj.items[i].comment).desired_rows(4).desired_width(f32::INFINITY)).changed() { self.proj.dirty = true; }
                                c[1].label(RichText::new(self.proj.items[i].path.clone()).weak());
                            });
                        }
                        _ => { ui.label("Wählen Sie im Explorer ein Bild aus (Doppelklick öffnet es hier)."); }
                    }
                }
                Tab::Check => {
                    let n = self.proj.items.len();
                    let ex = self.proj.items.iter().filter(|e| e.excluded).count();
                    ui.label(format!("{} Bilder im Projekt, davon {} ausgeschlossen.", n, ex));
                    let missing: Vec<&Entry> = self.proj.items.iter().filter(|e| !Path::new(&e.path).exists()).collect();
                    if missing.is_empty() { ui.colored_label(Color32::from_rgb(110, 200, 120), "OK: Alle Bilddateien sind vorhanden."); }
                    for e in missing { ui.colored_label(Color32::from_rgb(230, 170, 60), format!("Fehlt: {}", e.path)); }
                    let mut seen = HashSet::new();
                    for e in &self.proj.items { if !seen.insert(e.path.clone()) { ui.colored_label(Color32::from_rgb(230, 170, 60), format!("Doppelt: {}", e.path)); } }
                    if self.proj.items.iter().any(|e| e.title.trim().is_empty()) { ui.colored_label(Color32::from_rgb(230, 170, 60), "Hinweis: Einige Bilder haben keinen Titel."); }
                }
                Tab::Share => {
                    ui.label(format!("Ausgabeordner: {}", self.out_dir().display()));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Album erstellen").clicked() { acts.push(Act::Build); }
                        if ui.button("Im Browser öffnen").clicked() { acts.push(Act::OpenBrowser); }
                        if ui.button("Ordner öffnen").clicked() { acts.push(Act::OpenOut); }
                    });
                    ui.add_space(8.0);
                    ui.label("Zum Veröffentlichen den Ausgabeordner per FTP auf Ihren Webspace hochladen.");
                }
            }
        });

        let (mut ok, mut cancel) = (false, false);
        match self.modal {
            Modal::New => {
                egui::Window::new("Neues Projekt").collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                        ui.label("Album-Name");
                        ui.add(egui::TextEdit::singleline(&mut self.new_name).desired_width(320.0));
                        ui.label("Album-Beschreibung");
                        ui.add(egui::TextEdit::multiline(&mut self.new_desc).desired_rows(3).desired_width(320.0));
                        ui.label("Album-Tags");
                        ui.add(egui::TextEdit::singleline(&mut self.new_tags).hint_text("Beispiel: Natur, Makro, Portfolio").desired_width(320.0));
                        ui.horizontal(|ui| { if ui.button("Ok").clicked() { ok = true; } if ui.button("Abbrechen").clicked() { cancel = true; } });
                    });
                if ok {
                    let mut p = Project::default();
                    p.name = if self.new_name.trim().is_empty() { "Neues Album".into() } else { self.new_name.trim().to_string() };
                    p.desc = self.new_desc.clone();
                    p.tags = self.new_tags.clone();
                    p.dirty = true;
                    self.proj = p; self.sel.clear(); self.tex.clear(); self.modal = Modal::None;
                }
                if cancel { self.modal = Modal::None; }
            }
            Modal::Settings => {
                egui::Window::new("Einstellungen").collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                        ui.label("Albumtitel");
                        ui.add(egui::TextEdit::singleline(&mut self.proj.name).desired_width(360.0));
                        ui.label("Beschreibung");
                        ui.add(egui::TextEdit::singleline(&mut self.proj.desc).desired_width(360.0));
                        ui.label("Tags");
                        ui.add(egui::TextEdit::singleline(&mut self.proj.tags).desired_width(360.0));
                        ui.label("Ausgabeordner");
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut self.proj.out).desired_width(320.0));
                            if ui.button("...").clicked() { if let Some(p) = rfd::FileDialog::new().pick_folder() { self.proj.out = p.to_string_lossy().to_string(); } }
                        });
                        ui.add(egui::Slider::new(&mut self.proj.thumb, 100..=500).text("Vorschaugröße"));
                        ui.add(egui::Slider::new(&mut self.proj.size, 600..=4000).text("Bildgröße max."));
                        ui.add(egui::Slider::new(&mut self.proj.quality, 40..=100).text("JPEG-Qualität"));
                        ui.checkbox(&mut self.proj.originals, "Originale mitkopieren (Download)");
                        if ui.button("Ok").clicked() { ok = true; }
                    });
                if ok { self.proj.dirty = true; self.modal = Modal::None; }
            }
            Modal::About => {
                egui::Window::new("Über").collapsible(false).resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).show(ctx, |ui| {
                        ui.heading(NAME);
                        ui.label("Version 1.0 - Web-Galerien aus Fotos erstellen.");
                        ui.label("(c) Apollonios");
                        if ui.button("Schließen").clicked() { cancel = true; }
                    });
                if cancel { self.modal = Modal::None; }
            }
            Modal::None => {}
        }

        for a in acts { self.run(a, ctx); }
        let title = format!("{NAME} - {}{}", self.proj.name, if self.proj.dirty { " *" } else { "" });
        if title != self.last_title { ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone())); self.last_title = title; }
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0])
            .with_min_inner_size([820.0, 520.0]).with_title(NAME),
        ..Default::default()
    };
    eframe::run_native(NAME, opts, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
}
