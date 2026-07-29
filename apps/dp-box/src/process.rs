use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppStatus {
    Stopped,
    Running { pid: u32 },
    Error(String),
}

pub struct ProcessRegistry {
    children: Arc<Mutex<HashMap<String, Child>>>,
}

impl ProcessRegistry {
    pub fn new() -> Self {
        Self { children: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn launch(&self, name: &str, binary_path: &str) -> AppStatus {
        let mut guard = self.children.lock().unwrap();
        if let Some(mut existing) = guard.remove(name) {
            let _ = existing.kill();
            let _ = existing.wait();
        }
        match Command::new(binary_path).spawn() {
            Ok(child) => {
                let pid = child.id();
                guard.insert(name.to_string(), child);
                AppStatus::Running { pid }
            }
            Err(e) => AppStatus::Error(e.to_string()),
        }
    }

    pub fn stop(&self, name: &str) -> AppStatus {
        let mut guard = self.children.lock().unwrap();
        if let Some(mut child) = guard.remove(name) {
            let _ = child.kill();
            let _ = child.wait();
        }
        AppStatus::Stopped
    }

    pub fn poll_all(&self) -> Vec<String> {
        let mut guard = self.children.lock().unwrap();
        let mut exited = Vec::new();
        guard.retain(|name, child| match child.try_wait() {
            Ok(Some(_)) => { exited.push(name.clone()); false }
            Ok(None) => true,
            Err(_) => { exited.push(name.clone()); false }
        });
        exited
    }

    pub fn is_registered(&self, name: &str) -> bool {
        self.children.lock().unwrap().contains_key(name)
    }

    #[allow(dead_code)]
    pub fn kill_all(&self) {
        let mut guard = self.children.lock().unwrap();
        for (_, child) in guard.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        guard.clear();
    }

    #[allow(dead_code)]
    pub fn pid(&self, name: &str) -> Option<u32> {
        self.children.lock().unwrap().get(name).map(|c| c.id())
    }
}
