use super::*;

pub struct DirCheckRequestListener {
    pub base_path: PathBuf,
    pub known_check_requests: Vec<CheckRequest>,

    // These could all be oneshot channels I think
    // but maybe buffering is useful
    pub new_path_tx: mpsc::Sender<CheckRequest>,
    pub deleted_path_tx: mpsc::Sender<CheckRequest>,
    pub new_path_rx: Option<mpsc::Receiver<CheckRequest>>,
    pub deleted_path_rx: Option<mpsc::Receiver<CheckRequest>>,
}

impl DirCheckRequestListener {
    /// Constructs a new `Listener` that will monitor the specified path.
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<DirCheckRequestListener, Error> {
        let path_ref = path.as_ref();
        if !path_ref.exists() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path does not exist"),
            });
        }
        if !path_ref.is_dir() {
            return Err(Error::DirectoryIncorrect {
                source: io::Error::new(io::ErrorKind::NotFound, "Path is not a directory"),
            });
        }

        let (new_paths_tx, new_paths_rx) = mpsc::channel(100);
        let (deleted_paths_tx, deleted_paths_rx) = mpsc::channel(100);
        Ok(DirCheckRequestListener {
            base_path: path.as_ref().to_path_buf(),
            known_check_requests: Vec::new(),
            new_path_tx: new_paths_tx,
            deleted_path_tx: deleted_paths_tx,
            new_path_rx: Some(new_paths_rx),
            deleted_path_rx: Some(deleted_paths_rx),
        })
    }

    pub fn subscribe(&mut self) -> (mpsc::Receiver<CheckRequest>, mpsc::Receiver<CheckRequest>) {
        if self.new_path_rx.is_none() || self.deleted_path_rx.is_none() {
            panic!("Invariant violated: subscribe called after consuming the receivers");
        }

        (self.new_path_rx.take().unwrap(), self.deleted_path_rx.take().unwrap())
    }

    pub async fn update_check_entities(&mut self) -> Result<(), Error> {
        let new_check_paths = self.get_check_entities().await;
        let current_paths = self.known_check_requests.iter().map(|e| e.source.clone());
        let current: HashSet<CheckSource> = HashSet::from_iter(current_paths);
        let new: HashSet<CheckSource> = HashSet::from_iter(new_check_paths.iter().cloned());

        for entity in new.difference(&current) {
            let check_request = entity.to_check_request()?;
            self.known_check_requests.push(check_request.clone());
            // todo error handling
            self.new_path_tx.send(check_request).await.expect("Could send");
        }
        for entity in current.difference(&new) {
            // find the check request by path
            let check_request = self
                .known_check_requests
                .iter()
                .find(|e| e.source == *entity)
                .expect("couldn't find to-be-removed check")
                .clone();
            self.known_check_requests.retain(|e| e.source != *entity);
            // todo error handling
            self.deleted_path_tx.send(check_request).await.expect("Could send");
        }

        Ok(())
    }

    /// Retrieves all check entities from the base path that match the required check formats.
    pub async fn get_check_entities(&self) -> Vec<CheckSource> {
        let entries = WalkDir::new(&self.base_path).filter(|entry| async move {
            if let Some(true) = entry.path().file_name().map(|f| f.to_string_lossy().starts_with('.')) {
                return Filtering::IgnoreDir;
            }
            if is_check_entity(&entry).await {
                Filtering::Continue
            } else {
                Filtering::Ignore
            }
        });

        entries
            .filter_map(|e| async move {
                match e {
                    Ok(entry) => Some(CheckSource::Yaml(entry.path().to_path_buf())),
                    Err(e) => {
                        eprintln!("Error traversing files: {}", e);
                        None
                    }
                }
            })
            .collect()
            .await
    }
}

/// Determines if a directory entry is a valid check entity based on defined patterns.
async fn is_check_entity(entry: &DirEntry) -> bool {
    let path = entry.path();
    let file_type = entry.file_type().await.expect("Couldn't get file type");
    if file_type.is_file() {
        // Matches `./mycheck.yaml`
        return path.extension().unwrap_or_default() == "yaml";
    }

    if file_type.is_dir() {
        // Matches `./mycheck.d/conf.yaml`
        let conf_path = path.join("conf.yaml");
        return conf_path.exists()
            && path
                .file_name()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
                .ends_with("check.d");
    }
    false
}

pub struct DirCheckListenerContext {
    pub shutdown_handle: DynamicShutdownHandle,
    pub listener: DirCheckRequestListener,
}
