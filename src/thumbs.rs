//! Thumbnails for the library browser.
//!
//! Windows makes them: `IShellItemImageFactory` is the service Explorer uses. It works on network shares and for the video
//! formats the system can play (the 8K and VR180 files too), and Windows keeps its own cache of them, so asking again is fast.
//! A few worker threads ask for the ones the browser shows; the pictures land in one GPU texture cut into fixed slots, and the
//! UI draws from it (`QuadKind::Thumb`). Slots are given back, least recently shown first, when the texture is full.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};

pub const SLOT_W: u32 = 384;
pub const SLOT_H: u32 = 216;
const COLS: u32 = 10;
const ROWS: u32 = 12;
pub const ATLAS_W: u32 = COLS * SLOT_W;
pub const ATLAS_H: u32 = ROWS * SLOT_H;
const WORKERS: usize = 3;
/// Pictures uploaded to the GPU per frame (each is about 330 KB), so a folder opening does not hitch.
const UPLOADS_PER_FRAME: usize = 8;
/// A slot shown this recently is in use and is not given away.
const KEEP_FRAMES: u64 = 3;

/// What the browser gets for one file.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Thumb {
    /// In the texture: the whole picture's rectangle there (u0, v0, u1, v1) and its size in pixels.
    Ready { uv: [f32; 4], size: (u32, u32) },
    /// Being made, or waiting for a free slot.
    Loading,
    /// Windows has no thumbnail for it.
    Failed,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Loading,
    Ready(u32, u32),
    Failed,
}

struct Entry {
    slot: usize,
    state: State,
    used: u64,
}

/// Which file is in which slot. No threads and no GPU in here, so it can be tested.
pub struct Table {
    entries: HashMap<String, Entry>,
    free: Vec<usize>,
    frame: u64,
}

fn slot_uv(slot: usize, w: u32, h: u32) -> [f32; 4] {
    let (c, r) = (slot as u32 % COLS, slot as u32 / COLS);
    // half a texel in from every edge, so the linear filter never reads the next slot's picture
    let (x0, y0) = ((c * SLOT_W) as f32 + 0.5, (r * SLOT_H) as f32 + 0.5);
    [x0 / ATLAS_W as f32, y0 / ATLAS_H as f32, (x0 + w as f32 - 1.0) / ATLAS_W as f32, (y0 + h as f32 - 1.0) / ATLAS_H as f32]
}

fn slot_origin(slot: usize) -> (u32, u32) {
    (slot as u32 % COLS * SLOT_W, slot as u32 / COLS * SLOT_H)
}

impl Table {
    pub fn new(slots: usize) -> Self {
        Self { entries: HashMap::new(), free: (0..slots).rev().collect(), frame: 0 }
    }

    pub fn next_frame(&mut self) {
        self.frame += 1;
    }

    /// The state of `key`, marking it shown now. The bool says the caller must have it made (it just got a slot).
    pub fn get(&mut self, key: &str) -> (Thumb, bool) {
        let frame = self.frame;
        if let Some(e) = self.entries.get_mut(key) {
            e.used = frame;
            let t = match e.state {
                State::Loading => Thumb::Loading,
                State::Ready(w, h) => Thumb::Ready { uv: slot_uv(e.slot, w, h), size: (w, h) },
                State::Failed => Thumb::Failed,
            };
            return (t, false);
        }
        let Some(slot) = self.take_slot() else { return (Thumb::Loading, false) };
        self.entries.insert(key.to_string(), Entry { slot, state: State::Loading, used: frame });
        (Thumb::Loading, true)
    }

    /// A free slot, or the one whose picture was shown longest ago (if none is in use right now).
    fn take_slot(&mut self) -> Option<usize> {
        if let Some(s) = self.free.pop() {
            return Some(s);
        }
        let frame = self.frame;
        let oldest = self.entries.iter()
            .filter(|(_, e)| matches!(e.state, State::Ready(..)) && e.used + KEEP_FRAMES < frame)
            .min_by_key(|(_, e)| e.used)
            .map(|(k, _)| k.clone())?;
        self.entries.remove(&oldest).map(|e| e.slot)
    }

    /// Is `key` still being waited for (shown lately)? One that scrolled away is not worth making.
    pub fn wanted(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|e| e.state == State::Loading && e.used + KEEP_FRAMES + 2 >= self.frame)
    }

    /// Forget a request nobody wants any more and give its slot back.
    pub fn cancel(&mut self, key: &str) {
        if let Some(e) = self.entries.get(key) {
            if e.state == State::Loading {
                let slot = e.slot;
                self.entries.remove(key);
                self.free.push(slot);
            }
        }
    }

    /// The picture for `key` is made (`Some(size)`) or could not be (`None`). Returns the slot to upload into.
    pub fn finish(&mut self, key: &str, size: Option<(u32, u32)>) -> Option<usize> {
        let e = self.entries.get_mut(key)?;
        if e.state != State::Loading {
            return None;
        }
        match size {
            Some((w, h)) => {
                e.state = State::Ready(w, h);
                Some(e.slot)
            }
            None => {
                let slot = e.slot;
                e.state = State::Failed;
                self.free.push(slot); // a failure needs no slot
                None
            }
        }
    }
}

struct Loaded {
    key: String,
    picture: Option<(u32, u32, Vec<u8>)>,
}

struct Inner {
    table: Table,
    wanted: Vec<String>,
    ready: Vec<Loaded>,
    quit: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

pub struct Thumbs {
    shared: Arc<Shared>,
}

impl Thumbs {
    pub fn new() -> Arc<Self> {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                table: Table::new((COLS * ROWS) as usize),
                wanted: Vec::new(),
                ready: Vec::new(),
                quit: false,
            }),
            wake: Condvar::new(),
        });
        for i in 0..WORKERS {
            let shared = shared.clone();
            let spawned = std::thread::Builder::new().name(format!("thumbs-{i}")).spawn(move || worker(shared));
            if let Err(e) = spawned {
                crate::diag::log(&format!("thumbs: cannot start worker {i}: {e}"));
            }
        }
        Arc::new(Self { shared })
    }

    /// The thumbnail of the file at `path`; asks for it to be made the first time.
    pub fn get(&self, path: &str) -> Thumb {
        let Ok(mut g) = self.shared.inner.lock() else { return Thumb::Failed };
        let (thumb, make) = g.table.get(path);
        if make {
            g.wanted.push(path.to_string());
            self.shared.wake.notify_one();
        }
        thumb
    }

    /// Once per frame, before the UI draws: put the pictures that are made into the texture.
    pub fn upload_ready(&self, queue: &wgpu::Queue, texture: &wgpu::Texture) {
        let Ok(mut g) = self.shared.inner.lock() else { return };
        g.table.next_frame();
        let n = g.ready.len().min(UPLOADS_PER_FRAME);
        let batch: Vec<Loaded> = g.ready.drain(..n).collect();
        for done in batch {
            let size = done.picture.as_ref().map(|(w, h, _)| (*w, *h));
            let Some(slot) = g.table.finish(&done.key, size) else { continue };
            let Some((w, h, pixels)) = done.picture else { continue };
            let (x, y) = slot_origin(slot);
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &pixels,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
    }
}

impl Drop for Thumbs {
    fn drop(&mut self) {
        if let Ok(mut g) = self.shared.inner.lock() {
            g.quit = true;
        }
        self.shared.wake.notify_all();
    }
}

pub fn create_texture(device: &wgpu::Device) -> (wgpu::Texture, wgpu::TextureView) {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui_thumb_atlas"),
        size: wgpu::Extent3d { width: ATLAS_W, height: ATLAS_H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    (texture, view)
}

fn worker(shared: Arc<Shared>) {
    init_com();
    loop {
        // the newest request first: it is what the browser shows now
        let key = {
            let Ok(mut g) = shared.inner.lock() else { return };
            loop {
                if g.quit {
                    return;
                }
                match g.wanted.pop() {
                    Some(k) if g.table.wanted(&k) => break k,
                    Some(k) => g.table.cancel(&k),
                    None => match shared.wake.wait(g) {
                        Ok(next) => g = next,
                        Err(_) => return,
                    },
                }
            }
        };
        let started = std::time::Instant::now();
        let picture = load_thumbnail(&key);
        crate::diag::slow(&format!("thumbnail of {key}"), started, 2000);
        if let Ok(mut g) = shared.inner.lock() {
            g.ready.push(Loaded { key, picture });
        }
    }
}

#[cfg(windows)]
fn init_com() {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    // Shell items are happiest in a single-threaded apartment; this thread only ever makes thumbnails.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

#[cfg(not(windows))]
fn init_com() {}

/// One thumbnail as (width, height, RGBA pixels, opaque), at most `SLOT_W` x `SLOT_H`, or None if Windows has none.
#[cfg(windows)]
fn load_thumbnail(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::System::Com::IBindCtx;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        HGDIOBJ,
    };
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY};

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None::<&IBindCtx>).ok()?;
        let bitmap = factory.GetImage(SIZE { cx: SLOT_W as i32, cy: SLOT_H as i32 }, SIIGBF_THUMBNAILONLY).ok()?;
        let object = HGDIOBJ(bitmap.0);

        let mut info = BITMAP::default();
        if GetObjectW(object, std::mem::size_of::<BITMAP>() as i32, Some(&mut info as *mut BITMAP as *mut _)) == 0 {
            let _ = DeleteObject(object);
            return None;
        }
        let (w, h) = (info.bmWidth, info.bmHeight);
        // Windows may hand back a bigger picture than asked for (a square video comes as 384x384 for a 384x216 request)
        if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
            let _ = DeleteObject(object);
            return None;
        }
        let mut header = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top row first
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bitmap, 0, h as u32, Some(pixels.as_mut_ptr() as *mut _), &mut header, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        let _ = DeleteObject(object);
        if lines == 0 {
            return None;
        }
        for p in pixels.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA to RGBA
            p[3] = 255;
        }
        Some(shrink_to_fit(w as u32, h as u32, pixels))
    }
}

/// The picture made smaller (never bigger) until it fits one slot, keeping its shape; each new pixel is the average of the
/// old pixels it covers.
fn shrink_to_fit(w: u32, h: u32, pixels: Vec<u8>) -> (u32, u32, Vec<u8>) {
    if w <= SLOT_W && h <= SLOT_H {
        return (w, h, pixels);
    }
    let scale = (SLOT_W as f32 / w as f32).min(SLOT_H as f32 / h as f32);
    let nw = ((w as f32 * scale).floor() as u32).clamp(1, SLOT_W);
    let nh = ((h as f32 * scale).floor() as u32).clamp(1, SLOT_H);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let (y0, y1) = (y * h / nh, ((y + 1) * h / nh).max(y * h / nh + 1).min(h));
        for x in 0..nw {
            let (x0, x1) = (x * w / nw, ((x + 1) * w / nw).max(x * w / nw + 1).min(w));
            let mut sum = [0u32; 4];
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let i = ((sy * w + sx) * 4) as usize;
                    for c in 0..4 { sum[c] += pixels[i + c] as u32; }
                }
            }
            let n = (y1 - y0) * (x1 - x0);
            let o = ((y * nw + x) * 4) as usize;
            for c in 0..4 { out[o + c] = (sum[c] / n) as u8; }
        }
    }
    (nw, nh, out)
}

#[cfg(not(windows))]
fn load_thumbnail(_path: &str) -> Option<(u32, u32, Vec<u8>)> {
    None
}

#[cfg(test)]
mod thumbs_tests {
    use super::*;

    #[test]
    fn a_file_is_asked_for_once_and_comes_back_ready() {
        let mut t = Table::new(4);
        assert_eq!(t.get("a"), (Thumb::Loading, true));
        assert_eq!(t.get("a"), (Thumb::Loading, false), "already asked");
        let slot = t.finish("a", Some((384, 192))).expect("a slot to upload into");
        match t.get("a").0 {
            Thumb::Ready { uv, size } => {
                assert_eq!(size, (384, 192));
                let (x, y) = slot_origin(slot);
                assert!(uv[0] > x as f32 / ATLAS_W as f32 && uv[2] < (x + SLOT_W) as f32 / ATLAS_W as f32);
                assert!(uv[1] > y as f32 / ATLAS_H as f32 && uv[3] < (y + 192) as f32 / ATLAS_H as f32);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_picture_bigger_than_a_slot_is_shrunk_to_fit_and_a_small_one_is_left_alone() {
        // 384 x 384, left half black and right half white, every pixel opaque
        let (w, h) = (384u32, 384u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = if x < w / 2 { 0 } else { 255 };
                let i = ((y * w + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let (nw, nh, out) = shrink_to_fit(w, h, px);
        assert_eq!((nw, nh), (216, 216), "keeps its shape and fits the slot's height");
        assert_eq!(out.len(), (nw * nh * 4) as usize);
        assert_eq!(out[((10 * nw + 10) * 4) as usize], 0, "left stays black");
        assert_eq!(out[((10 * nw + nw - 10) * 4) as usize], 255, "right stays white");
        assert_eq!(out[3], 255, "still opaque");

        let small = vec![7u8; 100 * 50 * 4];
        assert_eq!(shrink_to_fit(100, 50, small.clone()), (100, 50, small));
        // a very wide picture is limited by the slot's width
        let wide = shrink_to_fit(1000, 100, vec![1u8; 1000 * 100 * 4]);
        assert_eq!((wide.0, wide.1), (384, 38));
    }

    #[test]
    fn a_failure_is_remembered_and_gives_the_slot_back() {
        let mut t = Table::new(1);
        assert_eq!(t.get("a"), (Thumb::Loading, true));
        assert_eq!(t.finish("a", None), None);
        assert_eq!(t.get("a"), (Thumb::Failed, false));
        assert_eq!(t.get("b"), (Thumb::Loading, true), "the failed file needs no slot");
    }

    #[test]
    fn a_full_table_gives_away_the_slot_shown_longest_ago_but_never_one_in_view() {
        let mut t = Table::new(2);
        for k in ["a", "b"] {
            t.get(k);
            t.finish(k, Some((100, 100)));
        }
        // both were shown this frame: nothing can be given away yet
        assert_eq!(t.get("c"), (Thumb::Loading, false));
        for _ in 0..5 {
            t.next_frame();
        }
        t.get("b"); // b is shown again, a is not
        assert_eq!(t.get("c"), (Thumb::Loading, true), "a's slot is taken");
        assert_eq!(t.get("a"), (Thumb::Loading, false), "a is gone; it has to wait for a slot now");
        assert!(matches!(t.get("b").0, Thumb::Ready { .. }), "b is still there");
    }

    #[test]
    fn a_request_that_scrolled_away_is_dropped_before_it_is_made() {
        let mut t = Table::new(2);
        t.get("a");
        assert!(t.wanted("a"));
        for _ in 0..(KEEP_FRAMES + 3) {
            t.next_frame();
        }
        assert!(!t.wanted("a"));
        t.cancel("a");
        assert_eq!(t.get("a"), (Thumb::Loading, true), "asking again starts over");
        assert_eq!(t.finish("never-asked", Some((1, 1))), None);
    }
}
