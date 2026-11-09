//! Threads d'arrière-plan : rendu des pages, extraction du texte et OCR.
//! L'interface ne fait que déposer des demandes et consommer des résultats, elle ne bloque jamais sur PDFium.

use crate::memory_manager::PageTextureKey;
use crate::ocr_engine::{HybridOcrEngine, OcrRecognizer, RecognizedOcrSegment};
use crate::pdf_engine::{LoadedPdfDocument, PdfEngine, TileRegion};
use crate::text_search::PageTextLayer;
use egui::{ColorImage, Context};
use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Documents gardés ouverts par le thread de rendu (onglet actif + changements rapides d'onglet).
const MAX_OPEN_RENDER_DOCS: usize = 3;
/// En mode Éco, le thread OCR se repose ce multiple du temps de travail entre deux pages.
const ECO_REST_FACTOR: u32 = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderJob {
    pub key: PageTextureKey,
    pub path: PathBuf,
    pub width_px: i32,
    pub height_px: i32,
    /// Si présent, seule cette zone est rendue, à `tile_scale` pixels par point PDF.
    pub tile: Option<TileRegion>,
    pub tile_scale: f32,
}

struct TextJob {
    tab_id: u64,
    path: PathBuf,
    page: usize,
}

pub struct OcrJob {
    pub tab_id: u64,
    pub path: PathBuf,
    pub page: usize,
    pub eco: bool,
}

pub struct OcrPageOutcome {
    pub tab_id: u64,
    pub page: usize,
    pub native_layer: Option<PageTextLayer>,
    pub images_found: usize,
    pub segments: Vec<RecognizedOcrSegment>,
    pub error: Option<String>,
}

pub enum WorkerEvent {
    Rendered {
        key: PageTextureKey,
        image: ColorImage,
    },
    RenderFailed {
        key: PageTextureKey,
    },
    TextLayer {
        tab_id: u64,
        layer: PageTextLayer,
    },
    OcrPage(OcrPageOutcome),
}

#[derive(Default)]
struct RenderQueue {
    render: Vec<RenderJob>,
    text: VecDeque<TextJob>,
    forget_tabs: Vec<u64>,
    render_in_progress: bool,
}

#[derive(Default)]
struct SharedState<T> {
    state: Mutex<T>,
    wake: Condvar,
}

impl<T> SharedState<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct BackgroundWorkers {
    render_queue: Arc<SharedState<RenderQueue>>,
    ocr_slot: Arc<SharedState<Option<OcrJob>>>,
    events: Receiver<WorkerEvent>,
}

impl BackgroundWorkers {
    pub fn spawn(ctx: Context, engine: &'static PdfEngine) -> Self {
        let render_queue = Arc::new(SharedState::<RenderQueue>::default());
        let ocr_slot = Arc::new(SharedState::<Option<OcrJob>>::default());
        let (tx, events) = channel();

        {
            let queue = render_queue.clone();
            let tx = tx.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("superpdf-render".into())
                .spawn(move || render_thread(engine, queue, tx, ctx))
                .expect("Impossible de démarrer le thread de rendu");
        }
        {
            let slot = ocr_slot.clone();
            std::thread::Builder::new()
                .name("superpdf-ocr".into())
                .spawn(move || ocr_thread(engine, slot, tx, ctx))
                .expect("Impossible de démarrer le thread OCR");
        }

        Self {
            render_queue,
            ocr_slot,
            events,
        }
    }

    /// Remplace les pages à rendre : les demandes devenues hors écran sont ainsi annulées d'elles-mêmes.
    pub fn set_render_jobs(&self, jobs: Vec<RenderJob>) {
        let mut queue = self.render_queue.lock();
        if queue.render != jobs {
            queue.render = jobs;
            self.render_queue.wake.notify_one();
        }
    }

    pub fn is_render_busy(&self) -> bool {
        let queue = self.render_queue.lock();
        queue.render_in_progress || !queue.render.is_empty()
    }

    /// `interactive` place la demande en tête (page survolée) plutôt qu'en file (indexation de recherche).
    pub fn request_text_layer(&self, tab_id: u64, path: PathBuf, page: usize, interactive: bool) {
        let job = TextJob { tab_id, path, page };
        let mut queue = self.render_queue.lock();
        if interactive {
            queue.text.push_front(job);
        } else {
            queue.text.push_back(job);
        }
        self.render_queue.wake.notify_one();
    }

    /// Ferme les documents que le thread de rendu garde ouverts pour cet onglet.
    pub fn forget_tab(&self, tab_id: u64) {
        let mut queue = self.render_queue.lock();
        queue.forget_tabs.push(tab_id);
        queue.text.retain(|j| j.tab_id != tab_id);
        self.render_queue.wake.notify_one();
    }

    /// Soumet une page à l'OCR. Retourne `false` si le thread est encore occupé.
    pub fn submit_ocr(&self, job: OcrJob) -> bool {
        let mut slot = self.ocr_slot.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(job);
        self.ocr_slot.wake.notify_one();
        true
    }

    pub fn try_next_event(&self) -> Option<WorkerEvent> {
        self.events.try_recv().ok()
    }
}

enum RenderThreadTask {
    Render(RenderJob),
    Text(TextJob),
}

fn render_thread(
    engine: &'static PdfEngine,
    queue: Arc<SharedState<RenderQueue>>,
    tx: Sender<WorkerEvent>,
    ctx: Context,
) {
    let mut docs: HashMap<u64, LoadedPdfDocument<'static>> = HashMap::new();
    let mut doc_order: VecDeque<u64> = VecDeque::new();
    // Alterne rendu et texte pour qu'un défilement continu n'affame pas le curseur texte.
    let mut prefer_text = false;

    loop {
        let (forget, task) = {
            let mut q = queue.lock();
            q.render_in_progress = false;
            while q.render.is_empty() && q.text.is_empty() && q.forget_tabs.is_empty() {
                q = queue.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
            let forget = std::mem::take(&mut q.forget_tabs);
            let take_text = !q.text.is_empty() && (prefer_text || q.render.is_empty());
            let task = if take_text {
                q.text.pop_front().map(RenderThreadTask::Text)
            } else if q.render.is_empty() {
                None
            } else {
                q.render_in_progress = true;
                Some(RenderThreadTask::Render(q.render.remove(0)))
            };
            (forget, task)
        };
        prefer_text = !prefer_text;

        for tab_id in forget {
            close_doc(&mut docs, &mut doc_order, tab_id);
        }
        let Some(task) = task else { continue };

        let (tab_id, path) = match &task {
            RenderThreadTask::Render(j) => (j.key.tab_id, j.path.clone()),
            RenderThreadTask::Text(j) => (j.tab_id, j.path.clone()),
        };
        if !docs.contains_key(&tab_id) {
            if let Ok(doc) = engine.open_lightweight(&path) {
                while doc_order.len() >= MAX_OPEN_RENDER_DOCS {
                    if let Some(oldest) = doc_order.front().copied() {
                        close_doc(&mut docs, &mut doc_order, oldest);
                    }
                }
                docs.insert(tab_id, doc);
                doc_order.push_back(tab_id);
            }
        }
        let doc = docs.get(&tab_id);

        let event = match task {
            RenderThreadTask::Render(job) => {
                let image = doc.and_then(|d| {
                    catch_unwind(AssertUnwindSafe(|| match job.tile {
                        Some(region) => {
                            d.render_page_tile(job.key.page_index, job.tile_scale, region)
                        }
                        None => d.render_page(job.key.page_index, job.width_px, job.height_px),
                    }))
                    .ok()
                    .and_then(Result::ok)
                });
                match image {
                    Some(image) => WorkerEvent::Rendered {
                        key: job.key,
                        image,
                    },
                    None => WorkerEvent::RenderFailed { key: job.key },
                }
            }
            RenderThreadTask::Text(job) => {
                let layer = doc
                    .and_then(|d| {
                        catch_unwind(AssertUnwindSafe(|| {
                            d.extract_single_page_text_layer(job.page)
                        }))
                        .ok()
                    })
                    .unwrap_or_else(|| PageTextLayer::new(job.page, Vec::new()));
                WorkerEvent::TextLayer {
                    tab_id: job.tab_id,
                    layer,
                }
            }
        };
        if tx.send(event).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn close_doc(
    docs: &mut HashMap<u64, LoadedPdfDocument<'static>>,
    order: &mut VecDeque<u64>,
    tab_id: u64,
) {
    order.retain(|id| *id != tab_id);
    if let Some(doc) = docs.remove(&tab_id) {
        let _lock = PdfEngine::lock_guard();
        drop(doc);
    }
}

fn ocr_thread(
    engine: &'static PdfEngine,
    slot: Arc<SharedState<Option<OcrJob>>>,
    tx: Sender<WorkerEvent>,
    ctx: Context,
) {
    loop {
        let job = {
            let mut guard = slot.lock();
            while guard.is_none() {
                guard = slot.wake.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            // Le créneau reste occupé pendant le traitement pour qu'une seule page soit en cours.
            let current = guard.as_ref().map(|j| OcrJob {
                tab_id: j.tab_id,
                path: j.path.clone(),
                page: j.page,
                eco: j.eco,
            });
            current.expect("créneau OCR vérifié non vide")
        };

        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| process_ocr_page(engine, &job)))
            .unwrap_or_else(|_| OcrPageOutcome {
                tab_id: job.tab_id,
                page: job.page,
                native_layer: None,
                images_found: 0,
                segments: Vec::new(),
                error: Some("erreur interne pendant l'OCR".to_string()),
            });
        let had_images = outcome.images_found > 0;

        if job.eco && had_images {
            let rest = started.elapsed() * ECO_REST_FACTOR;
            std::thread::sleep(rest.min(Duration::from_secs(4)));
        }

        *slot.lock() = None;
        if tx.send(WorkerEvent::OcrPage(outcome)).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

fn process_ocr_page(engine: &'static PdfEngine, job: &OcrJob) -> OcrPageOutcome {
    let mut outcome = OcrPageOutcome {
        tab_id: job.tab_id,
        page: job.page,
        native_layer: None,
        images_found: 0,
        segments: Vec::new(),
        error: None,
    };

    // Le document n'est ouvert que le temps d'extraire les pixels : l'inférence se fait sans verrou PDFium.
    let doc = match engine.open_lightweight(&job.path) {
        Ok(doc) => doc,
        Err(err) => {
            outcome.error = Some(format!("{err:#}"));
            return outcome;
        }
    };
    let native = doc.extract_single_page_text_layer(job.page);
    let (images_found, images) = HybridOcrEngine::collect_images_to_scan(&doc, job.page, &native);
    {
        let _lock = PdfEngine::lock_guard();
        drop(doc);
    }
    outcome.images_found = images_found;

    if !images.is_empty() {
        match OcrRecognizer::shared() {
            Ok(recognizer) => {
                outcome.segments = HybridOcrEngine::recognize_images(recognizer, images, &native);
            }
            Err(reason) => outcome.error = Some(reason.to_string()),
        }
    }
    outcome.native_layer = Some(native);
    outcome
}
