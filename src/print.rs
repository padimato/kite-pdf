//! Impression Windows : boîte de dialogue standard (imprimante, pages, copies), puis PDFium dessine
//! chaque page directement sur l'imprimante, en vectoriel, dans un thread d'arrière-plan.

use crate::pdf_engine::PdfEngine;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Handle = *mut c_void;

/// `PRINTDLGW` (alignement naturel en 64 bits).
#[repr(C)]
struct PrintDlgW {
    l_struct_size: u32,
    hwnd_owner: Handle,
    h_dev_mode: Handle,
    h_dev_names: Handle,
    h_dc: Handle,
    flags: u32,
    n_from_page: u16,
    n_to_page: u16,
    n_min_page: u16,
    n_max_page: u16,
    n_copies: u16,
    h_instance: Handle,
    l_cust_data: isize,
    lpfn_print_hook: Handle,
    lpfn_setup_hook: Handle,
    lp_print_template_name: *const u16,
    lp_setup_template_name: *const u16,
    h_print_template: Handle,
    h_setup_template: Handle,
}

#[repr(C)]
struct DocInfoW {
    cb_size: i32,
    lpsz_doc_name: *const u16,
    lpsz_output: *const u16,
    lpsz_datatype: *const u16,
    fw_type: u32,
}

const PD_RETURNDC: u32 = 0x0000_0100;
const PD_PAGENUMS: u32 = 0x0000_0002;
const PD_NOSELECTION: u32 = 0x0000_0004;
const PD_USEDEVMODECOPIESANDCOLLATE: u32 = 0x0004_0000;
const HORZRES: i32 = 8;
const VERTRES: i32 = 10;
/// Annotations dessinées, rendu destiné à l'impression.
const FPDF_ANNOT: i32 = 0x01;
const FPDF_PRINTING: i32 = 0x800;

#[link(name = "comdlg32")]
extern "system" {
    fn PrintDlgW(dialog: *mut PrintDlgW) -> i32;
}

#[link(name = "gdi32")]
extern "system" {
    fn StartDocW(dc: Handle, info: *const DocInfoW) -> i32;
    fn StartPage(dc: Handle) -> i32;
    fn EndPage(dc: Handle) -> i32;
    fn EndDoc(dc: Handle) -> i32;
    fn AbortDoc(dc: Handle) -> i32;
    fn DeleteDC(dc: Handle) -> i32;
    fn GetDeviceCaps(dc: Handle, index: i32) -> i32;
    fn CreateDCW(driver: *const u16, device: *const u16, port: *const u16, mode: *const c_void) -> Handle;
}

#[link(name = "kernel32")]
extern "system" {
    fn GlobalFree(memory: Handle) -> Handle;
}

#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> Handle;
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Imprimante et pages choisies (`pages` : index à partir de 0, inclus).
pub struct PrinterChoice {
    dc: Handle,
    pub pages: std::ops::RangeInclusive<usize>,
}

// Le contexte d'imprimante n'est utilisé que par le thread d'impression, une fois choisi.
unsafe impl Send for PrinterChoice {}

/// Boîte de dialogue d'impression de Windows. `None` si l'utilisateur annule.
pub fn choose_printer(page_count: usize) -> Option<PrinterChoice> {
    let last_page = page_count.clamp(1, u16::MAX as usize) as u16;
    let mut dialog = PrintDlgW {
        l_struct_size: std::mem::size_of::<PrintDlgW>() as u32,
        hwnd_owner: unsafe { GetForegroundWindow() },
        h_dev_mode: std::ptr::null_mut(),
        h_dev_names: std::ptr::null_mut(),
        h_dc: std::ptr::null_mut(),
        flags: PD_RETURNDC | PD_NOSELECTION | PD_USEDEVMODECOPIESANDCOLLATE,
        n_from_page: 1,
        n_to_page: last_page,
        n_min_page: 1,
        n_max_page: last_page,
        n_copies: 1,
        h_instance: std::ptr::null_mut(),
        l_cust_data: 0,
        lpfn_print_hook: std::ptr::null_mut(),
        lpfn_setup_hook: std::ptr::null_mut(),
        lp_print_template_name: std::ptr::null(),
        lp_setup_template_name: std::ptr::null(),
        h_print_template: std::ptr::null_mut(),
        h_setup_template: std::ptr::null_mut(),
    };
    let accepted = unsafe { PrintDlgW(&mut dialog) } != 0;
    unsafe {
        if !dialog.h_dev_mode.is_null() {
            GlobalFree(dialog.h_dev_mode);
        }
        if !dialog.h_dev_names.is_null() {
            GlobalFree(dialog.h_dev_names);
        }
    }
    if !accepted || dialog.h_dc.is_null() {
        return None;
    }
    let pages = if dialog.flags & PD_PAGENUMS != 0 {
        let from = dialog.n_from_page.max(1) as usize - 1;
        let to = (dialog.n_to_page.max(dialog.n_from_page) as usize - 1).min(page_count.saturating_sub(1));
        from..=to
    } else {
        0..=page_count.saturating_sub(1)
    };
    Some(PrinterChoice { dc: dialog.h_dc, pages })
}

/// Imprimante nommée, sans boîte de dialogue (vérifications automatisées).
pub fn named_printer(name: &str, page_count: usize) -> Option<PrinterChoice> {
    let device = wide(name);
    let dc = unsafe { CreateDCW(wide("WINSPOOL").as_ptr(), device.as_ptr(), std::ptr::null(), std::ptr::null()) };
    (!dc.is_null()).then(|| PrinterChoice { dc, pages: 0..=page_count.saturating_sub(1) })
}

#[derive(Debug, Clone, Default)]
pub struct PrintProgress {
    pub done: usize,
    pub total: usize,
    pub finished: bool,
    pub error: Option<String>,
}

/// Imprime `pdf` dans un thread. `output` : fichier de sortie (imprimante PDF, vérifications).
/// `cleanup` : fichier temporaire à supprimer une fois l'impression terminée.
pub fn print_in_background(
    engine: &'static PdfEngine,
    pdf: PathBuf,
    doc_name: String,
    choice: PrinterChoice,
    output: Option<PathBuf>,
    cleanup: Option<PathBuf>,
) -> Arc<Mutex<PrintProgress>> {
    let progress = Arc::new(Mutex::new(PrintProgress {
        total: choice.pages.clone().count(),
        ..Default::default()
    }));
    let shared = progress.clone();
    let _ = std::thread::Builder::new().name("kite-print".into()).spawn(move || {
        let result = print_pages(engine, &pdf, &doc_name, &choice, output.as_deref(), &shared);
        unsafe { DeleteDC(choice.dc) };
        if let Some(temp) = cleanup {
            let _ = std::fs::remove_file(temp);
        }
        let mut p = shared.lock().unwrap_or_else(|e| e.into_inner());
        p.finished = true;
        p.error = result.err();
    });
    progress
}

fn print_pages(
    engine: &'static PdfEngine,
    pdf: &Path,
    doc_name: &str,
    choice: &PrinterChoice,
    output: Option<&Path>,
    progress: &Arc<Mutex<PrintProgress>>,
) -> Result<(), String> {
    let dc = choice.dc;
    let name = wide(doc_name);
    let output_wide = output.map(|p| wide(&p.to_string_lossy()));
    let info = DocInfoW {
        cb_size: std::mem::size_of::<DocInfoW>() as i32,
        lpsz_doc_name: name.as_ptr(),
        lpsz_output: output_wide.as_ref().map_or(std::ptr::null(), |w| w.as_ptr()),
        lpsz_datatype: std::ptr::null(),
        fw_type: 0,
    };
    if unsafe { StartDocW(dc, &info) } <= 0 {
        return Err("l'imprimante a refusé le document".to_string());
    }
    // Zone imprimable de la feuille, en pixels de l'imprimante.
    let (area_w, area_h) = unsafe { (GetDeviceCaps(dc, HORZRES), GetDeviceCaps(dc, VERTRES)) };
    let outcome = engine.with_raw_document(pdf, |document| {
        for page_index in choice.pages.clone() {
            if unsafe { StartPage(dc) } <= 0 {
                return Err("impression interrompue par l'imprimante".to_string());
            }
            document.render_page_to_dc(page_index, dc, area_w, area_h, FPDF_ANNOT | FPDF_PRINTING)?;
            if unsafe { EndPage(dc) } <= 0 {
                return Err("impression interrompue par l'imprimante".to_string());
            }
            progress.lock().unwrap_or_else(|e| e.into_inner()).done += 1;
        }
        Ok(())
    });
    match outcome {
        Ok(()) => {
            unsafe { EndDoc(dc) };
            Ok(())
        }
        Err(e) => {
            unsafe { AbortDoc(dc) };
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_windows_structures_have_the_expected_size() {
        // Tailles de commdlg.h / wingdi.h en 64 bits : une erreur ici ferait planter la boîte de dialogue.
        assert_eq!(std::mem::size_of::<PrintDlgW>(), 120);
        assert_eq!(std::mem::size_of::<DocInfoW>(), 40);
    }
}
