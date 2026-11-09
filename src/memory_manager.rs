use egui::{Color32, ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};

/// Budget par défaut du cache global de textures GPU/RAM : 48 Mo max pour l'ensemble des 15+ onglets
pub const DEFAULT_TEXTURE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

/// Quad de surlignage multiplicatif (`/BM /Multiply`) permettant de colorer le fond blanc
/// tout en laissant le texte noir traverser le surligneur à 100 % en noir pur.
#[derive(Debug, Clone, Copy)]
pub struct MultiplicativeHighlightQuad {
    /// Rectangle normalisé dans la page `[x_min, y_min, x_max, y_max]` dans `0.0..=1.0`
    pub norm_rect: [f32; 4],
    pub color_rgb: (u8, u8, u8),
}

impl MultiplicativeHighlightQuad {
    /// Exprime ces surlignages dans le repère d'une tuile (`region` en coordonnées normalisées de page).
    pub fn remap_to_region(quads: &[Self], region: [f32; 4]) -> Vec<Self> {
        let width = (region[2] - region[0]).max(1e-6);
        let height = (region[3] - region[1]).max(1e-6);
        quads
            .iter()
            .filter(|q| {
                q.norm_rect[0] < region[2]
                    && q.norm_rect[2] > region[0]
                    && q.norm_rect[1] < region[3]
                    && q.norm_rect[3] > region[1]
            })
            .map(|q| Self {
                norm_rect: [
                    (q.norm_rect[0] - region[0]) / width,
                    (q.norm_rect[1] - region[1]) / height,
                    (q.norm_rect[2] - region[0]) / width,
                    (q.norm_rect[3] - region[1]) / height,
                ],
                color_rgb: q.color_rgb,
            })
            .collect()
    }

    pub fn hash_slice(quads: &[Self]) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        quads.len().hash(&mut hasher);
        for q in quads {
            for c in q.norm_rect {
                ((c * 10_000.0) as i32).hash(&mut hasher);
            }
            q.color_rgb.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// Applique un mélange multiplicatif (`/BM /Multiply` avec préservation d'encre noire) sur l'image de base.
/// - Les pixels sombres d'écriture (`luminance < 75`) traversent intégralement le surligneur sans aucun voile jaune.
/// - Les pixels clairs et blancs du papier prennent la teinte vive du surligneur (`pixel * couleur / 255`).
pub fn apply_multiplicative_highlights(
    base: &ColorImage,
    quads: &[MultiplicativeHighlightQuad],
) -> ColorImage {
    if quads.is_empty() {
        return base.clone();
    }
    let mut out = base.clone();
    let w = out.width();
    let h = out.height();
    if w == 0 || h == 0 {
        return out;
    }

    for q in quads {
        let x0 = ((q.norm_rect[0].clamp(0.0, 1.0) * w as f32).floor() as usize).min(w);
        let y0 = ((q.norm_rect[1].clamp(0.0, 1.0) * h as f32).floor() as usize).min(h);
        let x1 = ((q.norm_rect[2].clamp(0.0, 1.0) * w as f32).ceil() as usize).min(w);
        let y1 = ((q.norm_rect[3].clamp(0.0, 1.0) * h as f32).ceil() as usize).min(h);

        let hr = q.color_rgb.0 as u16;
        let hg = q.color_rgb.1 as u16;
        let hb = q.color_rgb.2 as u16;

        for y in y0..y1 {
            let row_offset = y * w;
            for x in x0..x1 {
                let p = &mut out.pixels[row_offset + x];
                let r = p.r() as u16;
                let g = p.g() as u16;
                let b = p.b() as u16;
                // Luminance perceptuelle rapide (0..=255)
                let lum = (r * 77 + g * 150 + b * 29) >> 8;
                // Si le pixel appartient au coeur sombre d'une lettre (< 75), on le laisse 100 % noir intact !
                if lum < 75 {
                    continue;
                }
                // Mélange multiplicatif ISO 32000 (/BM /Multiply) : blanc (255) -> couleur surligneur, gris -> transition douce
                let nr = ((r * hr) / 255) as u8;
                let ng = ((g * hg) / 255) as u8;
                let nb = ((b * hb) / 255) as u8;
                *p = Color32::from_rgba_unmultiplied(nr, ng, nb, p.a());
            }
        }
    }

    out
}

/// Clé unique identifiant une page rendue à un palier de zoom donné pour un onglet
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageTextureKey {
    pub tab_id: u64,
    pub page_index: usize,
    pub zoom_bucket: u32,
    /// `None` : la page entière. `Some((colonne, ligne))` : une tuile de la grille haute résolution.
    pub tile: Option<(u32, u32)>,
}

struct CachedTextureEntry {
    handle: Option<TextureHandle>,
    base_image: Option<ColorImage>,
    applied_highlights_hash: u64,
    byte_size: usize,
    last_access_tick: u64,
}

/// Gestionnaire centralisé de textures avec politique d'éviction LRU et plafond mémoire strict.
/// Permet d'ouvrir 15+ PDF en simultané sans jamais dépasser le budget RAM/VRAM alloué aux bitmaps.
pub struct LruTextureCache {
    entries: HashMap<PageTextureKey, CachedTextureEntry>,
    pub max_budget_bytes: usize,
    pub allocated_bytes: usize,
    access_clock: u64,
    pub total_hits: u64,
    pub total_misses: u64,
    pub total_evictions: u64,
}

impl Default for LruTextureCache {
    fn default() -> Self {
        Self::new(DEFAULT_TEXTURE_BUDGET_BYTES)
    }
}

impl LruTextureCache {
    pub fn new(max_budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_budget_bytes,
            allocated_bytes: 0,
            access_clock: 0,
            total_hits: 0,
            total_misses: 0,
            total_evictions: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Vérifie si une texture est présente et met à jour son horodatage LRU
    #[allow(dead_code)]
    pub fn get(&mut self, key: &PageTextureKey) -> Option<&TextureHandle> {
        self.access_clock += 1;
        let tick = self.access_clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_access_tick = tick;
            self.total_hits += 1;
            entry.handle.as_ref()
        } else {
            self.total_misses += 1;
            None
        }
    }

    /// Récupère la texture GPU en appliquant à la volée les surlignages multiplicatifs (`/BM /Multiply`)
    /// sur l'image de base si la liste des surlignages de la page a changé (sans aucun re-rendu PDFium).
    pub fn get_with_highlights(
        &mut self,
        key: &PageTextureKey,
        highlights: &[MultiplicativeHighlightQuad],
    ) -> Option<&TextureHandle> {
        self.access_clock += 1;
        let tick = self.access_clock;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_access_tick = tick;
            self.total_hits += 1;
            let desired_hash = MultiplicativeHighlightQuad::hash_slice(highlights);
            if entry.applied_highlights_hash != desired_hash {
                if let (Some(base), Some(tex)) = (&entry.base_image, &mut entry.handle) {
                    let composited = apply_multiplicative_highlights(base, highlights);
                    tex.set(composited, TextureOptions::LINEAR);
                    entry.applied_highlights_hash = desired_hash;
                }
            }
            entry.handle.as_ref()
        } else {
            self.total_misses += 1;
            None
        }
    }

    pub fn contains_key(&self, key: &PageTextureKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Insère une image rendue dans le cache GPU après avoir évincé les entrées les plus anciennes si le budget est dépassé.
    pub fn insert_color_image(
        &mut self,
        ctx: &Context,
        key: PageTextureKey,
        image: ColorImage,
        highlights: &[MultiplicativeHighlightQuad],
    ) {
        let byte_size = image.width() * image.height() * 4;
        self.evict_until_fits(byte_size, Some(key.tab_id));

        let applied_highlights_hash = MultiplicativeHighlightQuad::hash_slice(highlights);
        let composited = apply_multiplicative_highlights(&image, highlights);

        let tex = ctx.load_texture(
            format!(
                "superpdf_t{}_p{}_z{}_{:?}",
                key.tab_id, key.page_index, key.zoom_bucket, key.tile
            ),
            composited,
            TextureOptions::LINEAR,
        );

        self.access_clock += 1;
        if let Some(prev) = self.entries.insert(
            key,
            CachedTextureEntry {
                handle: Some(tex),
                base_image: Some(image),
                applied_highlights_hash,
                byte_size,
                last_access_tick: self.access_clock,
            },
        ) {
            self.allocated_bytes = self.allocated_bytes.saturating_sub(prev.byte_size);
        }
        self.allocated_bytes += byte_size;
    }

    /// Version sans contexte GPU (utilisée pour les tests unitaires déterministes de la politique LRU)
    #[cfg(test)]
    pub fn insert_raw_for_test(&mut self, key: PageTextureKey, byte_size: usize) {
        self.evict_until_fits(byte_size, Some(key.tab_id));
        self.access_clock += 1;
        if let Some(prev) = self.entries.insert(
            key,
            CachedTextureEntry {
                handle: None,
                base_image: None,
                applied_highlights_hash: 0,
                byte_size,
                last_access_tick: self.access_clock,
            },
        ) {
            self.allocated_bytes = self.allocated_bytes.saturating_sub(prev.byte_size);
        }
        self.allocated_bytes += byte_size;
    }

    /// Évince les textures les moins récemment utilisées jusqu'à libérer `needed_bytes`.
    /// Privilégie d'abord l'éviction des onglets inactifs avant celle de l'onglet actif.
    fn evict_until_fits(&mut self, needed_bytes: usize, active_tab_id: Option<u64>) {
        let target_limit = self.max_budget_bytes.saturating_sub(needed_bytes);

        while self.allocated_bytes > target_limit && !self.entries.is_empty() {
            // On cherche d'abord la plus ancienne texture d'un onglet inactif, sinon la plus ancienne globale
            let victim_key = self
                .entries
                .iter()
                .filter(|(k, _)| active_tab_id.map(|id| k.tab_id != id).unwrap_or(true))
                .min_by_key(|(_, v)| v.last_access_tick)
                .or_else(|| self.entries.iter().min_by_key(|(_, v)| v.last_access_tick))
                .map(|(k, _)| *k);

            if let Some(k) = victim_key {
                if let Some(removed) = self.entries.remove(&k) {
                    self.allocated_bytes = self.allocated_bytes.saturating_sub(removed.byte_size);
                    self.total_evictions += 1;
                }
            } else {
                break;
            }
        }
    }

    /// Libère immédiatement toutes les textures associées à un onglet (lors de sa fermeture ou de sa mise en hibernation)
    pub fn evict_tab(&mut self, tab_id: u64) -> usize {
        let mut freed_bytes = 0;
        let keys_to_remove: Vec<PageTextureKey> = self
            .entries
            .keys()
            .filter(|k| k.tab_id == tab_id)
            .copied()
            .collect();

        for k in keys_to_remove {
            if let Some(removed) = self.entries.remove(&k) {
                self.allocated_bytes = self.allocated_bytes.saturating_sub(removed.byte_size);
                freed_bytes += removed.byte_size;
                self.total_evictions += 1;
            }
        }
        freed_bytes
    }

    /// Ne garde pour cet onglet que la page entière au palier `page_bucket` et les tuiles au palier `tile_bucket`.
    pub fn prune_stale_zoom_buckets(
        &mut self,
        tab_id: u64,
        page_bucket: u32,
        tile_bucket: Option<u32>,
    ) {
        let stale_keys: Vec<PageTextureKey> = self
            .entries
            .keys()
            .filter(|k| {
                k.tab_id == tab_id
                    && match k.tile {
                        None => k.zoom_bucket != page_bucket,
                        Some(_) => Some(k.zoom_bucket) != tile_bucket,
                    }
            })
            .copied()
            .collect();

        for k in stale_keys {
            if let Some(removed) = self.entries.remove(&k) {
                self.allocated_bytes = self.allocated_bytes.saturating_sub(removed.byte_size);
                self.total_evictions += 1;
            }
        }
    }

    /// Palier de zoom déjà en cache le plus proche pour cette page (affiché en attendant le rendu exact).
    pub fn closest_cached_key(
        &self,
        tab_id: u64,
        page_index: usize,
        zoom_bucket: u32,
    ) -> Option<PageTextureKey> {
        self.entries
            .keys()
            .filter(|k| k.tab_id == tab_id && k.page_index == page_index && k.tile.is_none())
            .min_by_key(|k| k.zoom_bucket.abs_diff(zoom_bucket))
            .copied()
    }

    /// Purge intégralement le cache de textures
    pub fn clear(&mut self) {
        self.entries.clear();
        self.allocated_bytes = 0;
    }
}

#[repr(C)]
#[allow(non_snake_case)]
struct PROCESS_MEMORY_COUNTERS {
    cb: u32,
    PageFaultCount: u32,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
}

extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn K32GetProcessMemoryInfo(
        process: *mut c_void,
        ppsmemcounters: *mut PROCESS_MEMORY_COUNTERS,
        cb: u32,
    ) -> i32;
}

/// Retourne la consommation RAM réelle du processus Windows courant `(WorkingSetSize, PrivatePagefileUsage)` en octets.
pub fn query_process_memory_bytes() -> (usize, usize) {
    unsafe {
        let mut pmc: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        let ok = K32GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb);
        if ok != 0 {
            (pmc.WorkingSetSize, pmc.PagefileUsage)
        } else {
            (0, 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lru_cache_strictly_respects_memory_budget() {
        // Budget de 10 Mo
        let budget = 10 * 1024 * 1024;
        let mut cache = LruTextureCache::new(budget);
        let page_size = 3 * 1024 * 1024; // 3 Mo par page haute résolution

        // Insertion de 6 pages (18 Mo au total) -> le cache ne doit jamais dépasser 10 Mo (max 3 pages gardées)
        for p in 0..6 {
            cache.insert_raw_for_test(
                PageTextureKey {
                    tab_id: 1,
                    page_index: p,
                    zoom_bucket: 20,
                    tile: None,
                },
                page_size,
            );
            assert!(
                cache.allocated_bytes <= budget,
                "Le cache LRU ne doit jamais dépasser le budget ({} > {})",
                cache.allocated_bytes,
                budget
            );
        }

        assert_eq!(cache.len(), 3);
        assert_eq!(cache.total_evictions, 3);
        // Les pages 0, 1, 2 ont dû être évincées et les pages 3, 4, 5 conservées
        assert!(!cache.contains_key(&PageTextureKey {
            tab_id: 1,
            page_index: 0,
            zoom_bucket: 20,
            tile: None,
        }));
        assert!(cache.contains_key(&PageTextureKey {
            tab_id: 1,
            page_index: 5,
            zoom_bucket: 20,
            tile: None,
        }));
    }

    #[test]
    fn test_evict_hibernated_tab_frees_all_tab_textures() {
        let mut cache = LruTextureCache::new(50 * 1024 * 1024);
        cache.insert_raw_for_test(
            PageTextureKey {
                tab_id: 10,
                page_index: 0,
                zoom_bucket: 20,
                    tile: None,
            },
            4_000_000,
        );
        cache.insert_raw_for_test(
            PageTextureKey {
                tab_id: 20,
                page_index: 0,
                zoom_bucket: 20,
                    tile: None,
            },
            4_000_000,
        );
        assert_eq!(cache.allocated_bytes, 8_000_000);

        let freed = cache.evict_tab(10);
        assert_eq!(freed, 4_000_000);
        assert_eq!(cache.allocated_bytes, 4_000_000);
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn test_windows_process_memory_telemetry_is_valid() {
        let (working_set, private_bytes) = query_process_memory_bytes();
        assert!(
            working_set > 1024 * 1024,
            "La mesure WorkingSet Windows doit retourner une valeur réelle (> 1 Mo), reçu: {}",
            working_set
        );
        assert!(private_bytes > 1024 * 1024);
    }

    #[test]
    fn test_multiplicative_highlight_preserves_black_text_pixels() {
        // Image 4x1 : [Blanc papier, Noir pur écriture, Gris très sombre écriture, Blanc papier]
        let mut base = ColorImage::new([4, 1], Color32::WHITE);
        base.pixels[1] = Color32::from_rgb(0, 0, 0);
        base.pixels[2] = Color32::from_rgb(25, 25, 25);

        let quads = vec![MultiplicativeHighlightQuad {
            norm_rect: [0.0, 0.0, 1.0, 1.0],
            color_rgb: (255, 226, 52),
        }];

        let out = apply_multiplicative_highlights(&base, &quads);
        // Le fond blanc devient jaune fluo vif
        assert_eq!(out.pixels[0], Color32::from_rgb(255, 226, 52));
        // L'écriture noire et sombre traverse le surligneur à 100 % en noir pur sans jaunir !
        assert_eq!(out.pixels[1], Color32::from_rgb(0, 0, 0));
        assert_eq!(out.pixels[2], Color32::from_rgb(25, 25, 25));
    }
}
