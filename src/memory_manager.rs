use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::HashMap;
use std::ffi::c_void;

/// Budget par défaut du cache global de textures GPU/RAM : 48 Mo max pour l'ensemble des 15+ onglets
pub const DEFAULT_TEXTURE_BUDGET_BYTES: usize = 48 * 1024 * 1024;

/// Clé unique identifiant une page rendue à un palier de zoom donné pour un onglet
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageTextureKey {
    pub tab_id: u64,
    pub page_index: usize,
    pub zoom_bucket: u32,
}

struct CachedTextureEntry {
    handle: Option<TextureHandle>,
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

    pub fn contains_key(&self, key: &PageTextureKey) -> bool {
        self.entries.contains_key(key)
    }

    /// Insère une image rendue dans le cache GPU après avoir évincé les entrées les plus anciennes si le budget est dépassé.
    pub fn insert_color_image(
        &mut self,
        ctx: &Context,
        key: PageTextureKey,
        image: ColorImage,
    ) {
        let byte_size = image.width() * image.height() * 4;
        self.evict_until_fits(byte_size, Some(key.tab_id));

        let tex = ctx.load_texture(
            format!("superpdf_t{}_p{}_z{}", key.tab_id, key.page_index, key.zoom_bucket),
            image,
            TextureOptions::LINEAR,
        );

        self.access_clock += 1;
        if let Some(prev) = self.entries.insert(
            key,
            CachedTextureEntry {
                handle: Some(tex),
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

    /// Nettoie les paliers de zoom obsolètes d'un onglet pour ne garder que le palier de zoom courant
    pub fn prune_stale_zoom_buckets(&mut self, tab_id: u64, active_zoom_bucket: u32) {
        let stale_keys: Vec<PageTextureKey> = self
            .entries
            .keys()
            .filter(|k| k.tab_id == tab_id && k.zoom_bucket != active_zoom_bucket)
            .copied()
            .collect();

        for k in stale_keys {
            if let Some(removed) = self.entries.remove(&k) {
                self.allocated_bytes = self.allocated_bytes.saturating_sub(removed.byte_size);
                self.total_evictions += 1;
            }
        }
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
            zoom_bucket: 20
        }));
        assert!(cache.contains_key(&PageTextureKey {
            tab_id: 1,
            page_index: 5,
            zoom_bucket: 20
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
            },
            4_000_000,
        );
        cache.insert_raw_for_test(
            PageTextureKey {
                tab_id: 20,
                page_index: 0,
                zoom_bucket: 20,
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
}
