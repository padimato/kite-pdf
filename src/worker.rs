//! Threads d'arrière-plan : rendu des pages, extraction du texte et OCR.
//! L'interface ne fait que déposer des demandes et consommer des résultats, elle ne bloque jamais sur PDFium.

use crate::memory_manager::PageTextureKey;
use crate::ocr_engine::{HybridOcrEngine, OcrRecognizer, RecognizedOcrSegment};
use crate::annotations::{load_annotations_from_pdf, PdfAnnotation};
use crate::pdf_engine::{LoadedPdfDocument, PageDetails, PageDimensions, PdfEngine, TileRegion};
use crate::text_search::PageTextLayer;
use egui::{ColorImage, Context};
use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Documents gardés ouverts par le thread de rendu (onglet actif + changements rapides d'onglet).
const MAX_OPEN_RENDER_DOCS: usize = 3;
/// Avancement d'une lecture OCR approfondie, partagé entre son thread et l'interface.
#[derive(Debug, Clone)]
pub struct DeepOcrProgress {
    /// Pages restant à lire, dans l'ordre (les pages demandées en urgence sont mises devant)
    pub queue: VecDeque<usize>,
    /// Page en cours de lecture
    pub current: Option<usize>,
    /// Pages demandées
    pub total: usize,
    /// Pages traitées
    pub done: usize,
    /// Pages réellement lues par l'OCR
    pub read: usize,
    /// Temps de calcul cumulé des pages lues (pour estimer la durée restante)
    pub busy: Duration,
    pub paused: bool,
    pub cancelled: bool,
    pub finished: bool,
    pub error: Option<String>,
}

impl DeepOcrProgress {
    /// Fait passer `pages` en tête de la lecture en cours (sans doublon avec la file).
    pub fn prioritize(&mut self, pages: &[usize]) {
        for &page in pages.iter().rev() {
            if self.current == Some(page) {
                continue;
            }
            if let Some(index) = self.queue.iter().position(|&p| p == page) {
                self.queue.remove(index);
            } else {
                self.total += 1;
            }
            self.queue.push_front(page);
        }
    }

    /// Secondes par page lue : mesurées dès la première page, sinon `default_seconds`.
    pub fn seconds_per_page(&self, default_seconds: f32) -> f32 {
        if self.read > 0 {
            self.busy.as_secs_f32() / self.read as f32
        } else {
            default_seconds
        }
    }
}

/// Pages envoyées ensemble par l'analyse de fond d'un gros document.
const DETAILS_BATCH_PAGES: usize = 20;
/// L'analyse de fond cède la place au rendu et au texte au plus pendant cette durée par page.
const DETAILS_MAX_PAUSE: Duration = Duration::from_secs(2);
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
    /// Détails de pages d'un gros document, lus en tâche de fond après l'ouverture.
    PageDetails {
        tab_id: u64,
        pages: Vec<(usize, PageDetails)>,
    },
    /// Toutes les pages ont été lues.
    PageDetailsDone {
        tab_id: u64,
    },
    /// Une page lue par l'OCR approfondi.
    DeepOcrPage {
        tab_id: u64,
        page: usize,
        segments: Vec<RecognizedOcrSegment>,
    },
    /// Annotations relues du fichier en tâche de fond.
    AnnotationsLoaded {
        tab_id: u64,
        items: Vec<PdfAnnotation>,
    },
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
    sender: Sender<WorkerEvent>,
    ctx: Context,
    engine: &'static PdfEngine,
    /// Onglets fermés : leur analyse de fond s'arrête.
    closed_tabs: Arc<Mutex<HashSet<u64>>>,
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
            let tx = tx.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name("superpdf-ocr".into())
                .spawn(move || ocr_thread(engine, slot, tx, ctx))
                .expect("Impossible de démarrer le thread OCR");
        }

        Self {
            render_queue,
            ocr_slot,
            events,
            sender: tx,
            ctx,
            engine,
            closed_tabs: Arc::default(),
        }
    }

    /// Lit en tâche de fond les détails de chaque page d'un gros document (une page à la fois, pour
    /// que le rendu puisse s'intercaler).
    pub fn scan_page_details(&self, tab_id: u64, path: PathBuf, page_count: usize) {
        let (engine, tx, ctx, closed) =
            (self.engine, self.sender.clone(), self.ctx.clone(), self.closed_tabs.clone());
        let render_queue = self.render_queue.clone();
        let _ = std::thread::Builder::new()
            .name("superpdf-page-details".into())
            .spawn(move || {
                let Ok(doc) = engine.open_lightweight(&path) else {
                    return;
                };
                let mut batch = Vec::with_capacity(DETAILS_BATCH_PAGES);
                for page in 0..page_count {
                    if closed.lock().unwrap_or_else(|e| e.into_inner()).contains(&tab_id) {
                        break;
                    }
                    // Le verrou PDFium n'est pas équitable : sans cette pause, cette boucle le reprendrait
                    // aussitôt et la page à l'écran attendrait la fin de l'analyse (jusqu'à 15 s mesurées).
                    let pause_started = Instant::now();
                    while pause_started.elapsed() < DETAILS_MAX_PAUSE {
                        let busy = {
                            let queue = render_queue.lock();
                            queue.render_in_progress || !queue.render.is_empty() || !queue.text.is_empty()
                        };
                        if !busy {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    if let Some(details) = doc.page_details(page) {
                        batch.push((page, details));
                    }
                    if batch.len() >= DETAILS_BATCH_PAGES || page + 1 == page_count {
                        let _ = tx.send(WorkerEvent::PageDetails { tab_id, pages: std::mem::take(&mut batch) });
                        ctx.request_repaint();
                    }
                }
                let _ = tx.send(WorkerEvent::PageDetailsDone { tab_id });
                ctx.request_repaint();
                let _lock = PdfEngine::lock_guard();
                drop(doc);
            });
    }

    /// Lance l'OCR approfondi des `pages` (page entière). Les pages qui ont déjà du texte sont relues
    /// aussi (figures dessinées) : seules les lignes absentes du texte du fichier sont gardées.
    /// Le thread s'interrompt entre deux pages si `paused` ou `cancelled` est levé.
    pub fn start_deep_ocr(
        &self,
        tab_id: u64,
        path: PathBuf,
        pages: Vec<usize>,
    ) -> Arc<Mutex<DeepOcrProgress>> {
        let progress = Arc::new(Mutex::new(DeepOcrProgress {
            queue: pages.iter().copied().collect(),
            current: None,
            total: pages.len(),
            done: 0,
            read: 0,
            busy: Duration::ZERO,
            paused: false,
            cancelled: false,
            finished: false,
            error: None,
        }));
        let (engine, tx, ctx, closed, shared) = (
            self.engine,
            self.sender.clone(),
            self.ctx.clone(),
            self.closed_tabs.clone(),
            progress.clone(),
        );
        let lock = |p: &Arc<Mutex<DeepOcrProgress>>| -> DeepOcrProgress {
            p.lock().unwrap_or_else(|e| e.into_inner()).clone()
        };
        let _ = std::thread::Builder::new()
            .name("superpdf-deep-ocr".into())
            .spawn(move || {
                let finish = |error: Option<String>| {
                    let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
                    p.finished = true;
                    p.error = error;
                    ctx.request_repaint();
                };
                let doc = match engine.open_lightweight(&path) {
                    Ok(doc) => doc,
                    Err(e) => return finish(Some(format!("{e:#}"))),
                };
                // Le moteur des figures est en pause pendant l'OCR approfondi : sa mémoire est rendue.
                OcrRecognizer::release_shared();
                let mut error = None;
                loop {
                    let page = {
                        let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
                        p.current = p.queue.pop_front();
                        p.current
                    };
                    let Some(page) = page else {
                        break;
                    };
                    loop {
                        let state = lock(&shared);
                        if state.cancelled || !state.paused {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    let tab_closed = closed.lock().unwrap_or_else(|e| e.into_inner()).contains(&tab_id);
                    if lock(&shared).cancelled || tab_closed {
                        break;
                    }
                    let started = Instant::now();
                    let native = doc.extract_single_page_text_layer(page);
                    match crate::ocr_engine::recognize_full_page(&doc, page, &native) {
                        Ok(segments) => {
                            {
                                let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
                                p.done += 1;
                                p.read += 1;
                                p.busy += started.elapsed();
                                p.current = None;
                            }
                            let _ = tx.send(WorkerEvent::DeepOcrPage { tab_id, page, segments });
                            ctx.request_repaint();
                        }
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
                {
                    let _lock = PdfEngine::lock_guard();
                    drop(doc);
                }
                OcrRecognizer::release_full_page();
                finish(error);
            });
        progress
    }

    /// Relit les annotations du fichier en tâche de fond (gros document).
    pub fn load_annotations(&self, tab_id: u64, path: PathBuf, dims: Vec<PageDimensions>) {
        let (tx, ctx) = (self.sender.clone(), self.ctx.clone());
        let _ = std::thread::Builder::new()
            .name("superpdf-annotations".into())
            .spawn(move || {
                if let Ok(items) = load_annotations_from_pdf(&path, &dims) {
                    let _ = tx.send(WorkerEvent::AnnotationsLoaded { tab_id, items });
                    ctx.request_repaint();
                }
            });
    }

    /// L'onglet est fermé : ses analyses de fond s'arrêtent.
    pub fn tab_closed(&self, tab_id: u64) {
        self.closed_tabs.lock().unwrap_or_else(|e| e.into_inner()).insert(tab_id);
        self.forget_tab(tab_id);
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
    let (images_found, mut images) = HybridOcrEngine::collect_images_to_scan(&doc, job.page, &native);
    // Page sans texte (scan, texte dessiné) : lire ses seules images n'a pas de sens, c'est le rôle
    // de l'OCR approfondi, proposé pour ces documents.
    if crate::ocr_engine::native_text_chars(&native) <= crate::ocr_engine::PAGE_WITHOUT_TEXT_MAX_CHARS {
        images.clear();
    }
    {
        let _lock = PdfEngine::lock_guard();
        drop(doc);
    }
    outcome.images_found = images_found;

    if !images.is_empty() {
        match OcrRecognizer::shared() {
            Ok(recognizer) => {
                outcome.segments = HybridOcrEngine::recognize_images(&recognizer, images, &native);
            }
            Err(reason) => outcome.error = Some(reason.to_string()),
        }
    }
    outcome.native_layer = Some(native);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_ocr_requested_pages_jump_the_queue() {
        let mut progress = DeepOcrProgress {
            queue: (0..10).collect(),
            current: Some(42),
            total: 11,
            done: 1,
            read: 1,
            busy: Duration::ZERO,
            paused: true,
            cancelled: false,
            finished: false,
            error: None,
        };
        // Page 7 déjà prévue : déplacée en tête sans être comptée deux fois ; page 300 ajoutée ;
        // la page en cours de lecture n'est pas relue.
        progress.prioritize(&[300, 7, 42]);
        assert_eq!(progress.queue.iter().take(3).copied().collect::<Vec<_>>(), vec![300, 7, 0]);
        assert_eq!(progress.queue.len(), 11);
        assert_eq!(progress.total, 12);
    }
}
