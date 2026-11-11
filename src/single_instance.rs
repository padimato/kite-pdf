//! Une seule fenêtre par utilisateur : un PDF ouvert depuis l'Explorateur alors que l'application
//! tourne déjà est transmis à la fenêtre existante (nouvel onglet), puis le second processus se ferme.
//!
//! La première instance crée un tube nommé propre à l'utilisateur ; les suivantes y écrivent les
//! chemins à ouvrir, un par ligne (message vide : simplement ramener la fenêtre au premier plan).

use std::ffi::c_void;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

type Handle = *mut c_void;

const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
const PIPE_ACCESS_INBOUND: u32 = 0x0000_0001;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0;
const PIPE_WAIT: u32 = 0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x0000_0008;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_NO_DATA: u32 = 232;
const ASFW_ANY: u32 = u32::MAX;

#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer: u32,
        in_buffer: u32,
        default_timeout: u32,
        security: *mut c_void,
    ) -> Handle;
    fn ConnectNamedPipe(pipe: Handle, overlapped: *mut c_void) -> i32;
    fn DisconnectNamedPipe(pipe: Handle) -> i32;
    fn ReadFile(file: Handle, buffer: *mut u8, len: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
    fn WaitNamedPipeW(name: *const u16, timeout_ms: u32) -> i32;
    fn GetLastError() -> u32;
}

#[link(name = "user32")]
extern "system" {
    fn AllowSetForegroundWindow(process_id: u32) -> i32;
}

/// Ce qu'une autre instance a demandé : ouvrir ces PDF (liste vide : seulement ramener la fenêtre).
pub struct Request {
    pub paths: Vec<PathBuf>,
}

static PENDING: Mutex<Vec<Request>> = Mutex::new(Vec::new());
static REPAINT: OnceLock<egui::Context> = OnceLock::new();

pub enum Role {
    /// Aucune autre fenêtre : cette instance reçoit désormais les demandes des suivantes.
    Primary,
    /// La demande a été transmise à la fenêtre existante : ce processus peut se fermer.
    Forwarded,
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain([0]).collect()
}

fn pipe_name() -> String {
    let user: String = std::env::var("USERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    // Une compilation de développement ne se mélange pas avec la version installée.
    let flavour = if cfg!(debug_assertions) { "-dev" } else { "" };
    format!(r"\\.\pipe\KitePDF-{user}{flavour}")
}

/// Devient la fenêtre principale, ou confie `paths` à celle qui existe déjà.
pub fn acquire(paths: &[PathBuf]) -> Role {
    acquire_on(&pipe_name(), paths)
}

fn acquire_on(pipe_path: &str, paths: &[PathBuf]) -> Role {
    let name = wide(pipe_path);
    // SAFETY: nom terminé par zéro ; aucun descripteur de sécurité (celui par défaut réserve
    // l'écriture au même utilisateur).
    let pipe = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            0,
            64 * 1024,
            0,
            std::ptr::null_mut(),
        )
    };
    if pipe != INVALID_HANDLE_VALUE {
        let pipe = pipe as usize;
        let _ = std::thread::Builder::new()
            .name("kite-single-instance".into())
            .spawn(move || listen(pipe as Handle));
        return Role::Primary;
    }
    // Le tube existe déjà (refus d'accès ou « occupé » selon la version de Windows) : une autre
    // fenêtre est ouverte, on lui confie la demande.
    if send(pipe_path, paths) {
        return Role::Forwarded;
    }
    // Tube indisponible pour une autre raison : l'application s'ouvre normalement, sans partage.
    Role::Primary
}

fn send(pipe_path: &str, paths: &[PathBuf]) -> bool {
    let name = wide(pipe_path);
    let message: String = paths
        .iter()
        .map(|p| std::path::absolute(p).unwrap_or_else(|_| p.clone()))
        .map(|p| format!("{}\n", p.display()))
        .collect();
    // Le tube peut être brièvement occupé par une autre demande (plusieurs fichiers ouverts d'un coup).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match std::fs::OpenOptions::new().write(true).open(pipe_path) {
            Ok(mut pipe) => {
                // Le processus lancé par l'utilisateur a le droit de passer la main au premier plan.
                // SAFETY: appel sans pointeur.
                unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                return pipe.write_all(message.as_bytes()).is_ok();
            }
            // SAFETY: nom terminé par zéro.
            Err(_) => {
                if unsafe { WaitNamedPipeW(name.as_ptr(), 500) } == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            }
        }
    }
    false
}

fn listen(pipe: Handle) {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: `pipe` reste ouvert pendant toute la vie du processus.
        // Un client arrivé (ou déjà reparti, `ERROR_NO_DATA`) avant cet appel compte comme connecté :
        // ce qu'il a écrit reste lisible, et le tube doit de toute façon être déconnecté ensuite.
        let connected = unsafe { ConnectNamedPipe(pipe, std::ptr::null_mut()) } != 0
            || matches!(unsafe { GetLastError() }, ERROR_PIPE_CONNECTED | ERROR_NO_DATA);
        if !connected {
            unsafe { DisconnectNamedPipe(pipe) };
            std::thread::sleep(std::time::Duration::from_millis(200));
            continue;
        }
        let mut message = Vec::new();
        loop {
            let mut read = 0u32;
            // SAFETY: `buffer` vit le temps de l'appel, sa longueur est passée.
            let ok = unsafe {
                ReadFile(pipe, buffer.as_mut_ptr(), buffer.len() as u32, &mut read, std::ptr::null_mut())
            } != 0;
            if !ok || read == 0 {
                break;
            }
            message.extend_from_slice(&buffer[..read as usize]);
        }
        // SAFETY: déconnecte le client pour accepter le suivant.
        unsafe { DisconnectNamedPipe(pipe) };
        push(parse(&String::from_utf8_lossy(&message)));
    }
}

fn parse(message: &str) -> Request {
    Request {
        paths: message.lines().map(str::trim).filter(|l| !l.is_empty()).map(PathBuf::from).collect(),
    }
}

fn push(request: Request) {
    if let Ok(mut pending) = PENDING.lock() {
        pending.push(request);
    }
    if let Some(ctx) = REPAINT.get() {
        ctx.request_repaint();
    }
}

/// Contexte à réveiller quand une demande arrive (fenêtre en veille).
pub fn set_repaint_context(ctx: &egui::Context) {
    let _ = REPAINT.set(ctx.clone());
}

/// Demandes reçues depuis le dernier appel.
pub fn take_requests() -> Vec<Request> {
    PENDING.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_request() {
        let request = parse("C:\\docs\\a.pdf\r\n\nD:\\b c.pdf\n");
        assert_eq!(request.paths, vec![PathBuf::from("C:\\docs\\a.pdf"), PathBuf::from("D:\\b c.pdf")]);
        assert!(parse("").paths.is_empty());
    }

    #[test]
    fn test_second_instance_forwards_paths() {
        let pipe = format!(r"\\.\pipe\KitePDF-test-{}", std::process::id());
        assert!(matches!(acquire_on(&pipe, &[]), Role::Primary));
        let file = PathBuf::from(r"C:\docs\rapport final.pdf");
        assert!(matches!(acquire_on(&pipe, &[file.clone()]), Role::Forwarded));
        assert!(matches!(acquire_on(&pipe, &[]), Role::Forwarded));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut received = Vec::new();
        while received.len() < 2 && std::time::Instant::now() < deadline {
            received.extend(take_requests());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].paths, vec![file]);
        assert!(received[1].paths.is_empty());
    }
}
