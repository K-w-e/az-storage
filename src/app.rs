//! Stato UI e state machine (focus, albero risorse, modali).

use crate::auth;
use crate::config::{self, AttachedAccount, Config};
use crate::model::*;
use crate::worker::{Command, DownloadSpec, Event, TransferProgress};
use base64::Engine;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::widgets::ListState;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Tipi di stato
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivePanel {
    SidebarTree,
    MainItemList,
    PreviewInspector,
}

#[derive(Debug, Clone)]
pub enum PendingAction {
    DeleteBlob {
        account_id: String,
        container: String,
        blob: String,
        prefix: String,
    },
    DeleteMessage {
        account_id: String,
        queue: String,
        id: String,
    },
    ClearQueue {
        account_id: String,
        queue: String,
    },
    DetachAccount {
        profile: String,
    },
}

#[derive(Debug, Default)]
pub struct AddForm {
    pub name: String,
    pub conn: String,
    pub field: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TransferKind {
    Download {
        account_id: String,
        container: String,
        spec: DownloadSpec,
    },
    Upload {
        account_id: String,
        container: String,
        prefix: String,
    },
}

#[derive(Debug)]
pub struct TransferForm {
    pub kind: TransferKind,
    pub path: String,
    pub overwrite: bool,
    pub error: Option<String>,
}

pub enum ModalState {
    None,
    AddConnectionStringDialog(AddForm),
    ConfirmDeleteDialog {
        target: String,
        action: PendingAction,
    },
    ErrorPopup(String),
    TransferDialog(TransferForm),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKey {
    SubsRoot,
    AttachedRoot,
    Sub(String),
    Account(String),
    Blobs(String),
    Queues(String),
    Container(String, String),
    Queue(String, String),
    Info(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Load {
    Idle,
    Loading,
    Loaded,
    Failed,
}

#[derive(Debug)]
pub struct TreeNode {
    pub key: NodeKey,
    pub label: String,
    pub detail: String,
    pub leaf: bool,
    pub expanded: bool,
    pub load: Load,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(key: NodeKey, label: impl Into<String>, leaf: bool) -> Self {
        Self {
            key,
            label: label.into(),
            detail: String::new(),
            leaf,
            expanded: false,
            load: Load::Idle,
            children: Vec::new(),
        }
    }
    fn info(text: impl Into<String>) -> Self {
        let t: String = text.into();
        Self::new(NodeKey::Info(t.clone()), t, true)
    }
    fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = d.into();
        self
    }
    /// Imposta i figli dopo un caricamento (o un placeholder se vuoto).
    fn set_children(&mut self, children: Vec<TreeNode>) {
        self.children = if children.is_empty() {
            vec![TreeNode::info("(empty)")]
        } else {
            children
        };
        self.load = Load::Loaded;
    }
    fn set_error(&mut self, e: &str) {
        self.children = vec![TreeNode::info(format!("✗ {e}"))];
        self.load = Load::Failed;
        self.expanded = true;
    }
    fn set_loading(&mut self) {
        self.children = vec![TreeNode::info("⏳ loading…")];
        self.load = Load::Loading;
        self.expanded = true;
    }
}

fn find_in<'a>(nodes: &'a mut [TreeNode], key: &NodeKey) -> Option<&'a mut TreeNode> {
    for n in nodes {
        if n.key == *key {
            return Some(n);
        }
        if let Some(f) = find_in(&mut n.children, key) {
            return Some(f);
        }
    }
    None
}

pub struct Row {
    pub path: Vec<usize>,
    pub depth: usize,
}

fn flatten(nodes: &[TreeNode], depth: usize, prefix: &mut Vec<usize>, out: &mut Vec<Row>) {
    for (i, n) in nodes.iter().enumerate() {
        prefix.push(i);
        out.push(Row {
            path: prefix.clone(),
            depth,
        });
        if n.expanded {
            flatten(&n.children, depth + 1, prefix, out);
        }
        prefix.pop();
    }
}

#[derive(Debug, Clone)]
pub enum Entry {
    Parent,
    Dir(String),
    Blob(BlobItem),
}

pub enum MainView {
    Empty,
    Blobs {
        account_id: String,
        container: String,
        prefix: String,
        entries: Vec<Entry>,
        loading: bool,
        error: Option<String>,
    },
    Messages {
        account_id: String,
        queue: String,
        messages: Vec<QueueMessage>,
        loading: bool,
        error: Option<String>,
    },
}

/// Ordinamento della lista blob (le cartelle restano sempre prima dei file).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    #[default]
    NameAsc,
    NameDesc,
    SizeDesc,
    Newest,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::NameAsc => SortMode::NameDesc,
            SortMode::NameDesc => SortMode::SizeDesc,
            SortMode::SizeDesc => SortMode::Newest,
            SortMode::Newest => SortMode::NameAsc,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            SortMode::NameAsc => "name ↑",
            SortMode::NameDesc => "name ↓",
            SortMode::SizeDesc => "size ↓",
            SortMode::Newest => "newest",
        }
    }
}

/// Contenuto di una cartella mostrato nel pannello preview (senza aprirla).
pub enum DirPreview {
    Loading,
    Ready(Vec<Entry>),
    Failed(String),
}

/// Attesa prima di caricare la preview di una cartella (evita richieste
/// a raffica scorrendo velocemente la lista).
const DIR_PREVIEW_DELAY: Duration = Duration::from_millis(150);
const DIR_CACHE_MAX: usize = 256;

pub enum PreviewState {
    Loading,
    Ready { text: String, truncated: bool },
    Failed(String),
}

pub enum CliState {
    Checking,
    Active(String),
    Unavailable(String),
}

pub struct App {
    pub config: Config,
    pub accounts: HashMap<String, StorageAccountNode>,
    pub subs: HashMap<String, Subscription>,
    pub tree: Vec<TreeNode>,
    pub tree_state: ListState,
    tree_key: Option<NodeKey>,
    pub panel: ActivePanel,
    pub modal: ModalState,
    pub main: MainView,
    pub main_state: ListState,
    /// Filtro testuale sulla lista centrale (case-insensitive, termini in AND).
    pub filter: String,
    pub filter_editing: bool,
    pub sort: SortMode,
    /// Cache delle preview cartella, chiave `account/container/prefix`.
    pub dir_cache: HashMap<String, DirPreview>,
    dir_sel: Option<(String, Instant)>,
    pub preview_key: Option<String>,
    pub preview: Option<PreviewState>,
    pub preview_scroll: u16,
    pub cli: CliState,
    pub status: Option<(String, Instant)>,
    pub busy: Arc<AtomicUsize>,
    pub spinner: usize,
    pub should_quit: bool,
    pub mouse_enabled: bool,
    /// Aree cliccabili calcolate all'ultimo render.
    pub hit: HitAreas,
    last_click: Option<(Instant, ActivePanel, usize)>,
    pub transfers: BTreeMap<u64, TransferProgress>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct HitAreas {
    pub tree: Rect,
    pub tree_list: Rect,
    pub main: Rect,
    pub main_list: Rect,
    pub preview: Rect,
    pub modal: Rect,
    pub form_fields: [Rect; 2],
    pub confirm_yes: Rect,
    pub confirm_no: Rect,
}

const DOUBLE_CLICK: Duration = Duration::from_millis(400);

// ---------------------------------------------------------------------------
// Costruzione
// ---------------------------------------------------------------------------

fn account_node(acct: &StorageAccountNode) -> TreeNode {
    let id = &acct.id;
    let label = match &acct.source {
        AccountSource::AttachedManual { profile_name } => profile_name.clone(),
        AccountSource::AzureSubscription { .. } => acct.name.clone(),
    };
    let detail = match &acct.source {
        AccountSource::AttachedManual { .. } => format!("{} · {}", acct.name, acct.kind),
        AccountSource::AzureSubscription { resource_group, .. } => {
            format!("{} · {resource_group}", acct.kind)
        }
    };
    let mut n = TreeNode::new(NodeKey::Account(id.clone()), label, false).with_detail(detail);
    if !acct.blob_endpoint.is_empty() {
        n.children.push(TreeNode::new(
            NodeKey::Blobs(id.clone()),
            "󰆼 Blob Containers",
            false,
        ));
    }
    if !acct.queue_endpoint.is_empty() {
        n.children.push(TreeNode::new(
            NodeKey::Queues(id.clone()),
            "󰒲 Storage Queues",
            false,
        ));
    }
    n.load = Load::Loaded;
    n
}

impl App {
    pub fn new(config: Config, busy: Arc<AtomicUsize>) -> (Self, Vec<Command>) {
        let mut subs_root = TreeNode::new(
            NodeKey::SubsRoot,
            "󰌒 Azure Subscriptions (Azure CLI)",
            false,
        );
        subs_root.set_loading();
        let mut attached = TreeNode::new(
            NodeKey::AttachedRoot,
            "󰌷 Attached Storage Accounts (Local Config)",
            false,
        );
        attached.expanded = true;
        attached.load = Load::Loaded;

        let mut app = App {
            config,
            accounts: HashMap::new(),
            subs: HashMap::new(),
            tree: Vec::new(),
            tree_state: ListState::default().with_selected(Some(0)),
            tree_key: Some(NodeKey::SubsRoot),
            panel: ActivePanel::SidebarTree,
            modal: ModalState::None,
            main: MainView::Empty,
            main_state: ListState::default(),
            filter: String::new(),
            filter_editing: false,
            sort: SortMode::default(),
            dir_cache: HashMap::new(),
            dir_sel: None,
            preview_key: None,
            preview: None,
            preview_scroll: 0,
            cli: CliState::Checking,
            status: None,
            busy,
            spinner: 0,
            should_quit: false,
            mouse_enabled: true,
            hit: HitAreas::default(),
            last_click: None,
            transfers: BTreeMap::new(),
        };
        for a in app.config.attached_accounts.clone() {
            attached.children.push(app.attached_node(&a));
        }
        if attached.children.is_empty() {
            attached
                .children
                .push(TreeNode::info("(none — press [a] to attach)"));
        }
        app.tree = vec![subs_root, attached];
        (app, vec![Command::DiscoverCli])
    }

    fn attached_node(&mut self, a: &AttachedAccount) -> TreeNode {
        match auth::parse_connection_string(&a.name, &a.connection_string) {
            Ok(acct) => {
                let n = account_node(&acct);
                self.accounts.insert(acct.id.clone(), acct);
                n
            }
            Err(e) => TreeNode::info(format!("✗ {}: {e}", a.name)),
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst) > 0
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(6))
            .map(|(s, _)| s.as_str())
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    // ------------------------------ Tree -----------------------------------

    pub fn rows(&self) -> Vec<Row> {
        let mut out = Vec::new();
        flatten(&self.tree, 0, &mut Vec::new(), &mut out);
        out
    }

    pub fn node_at(&self, path: &[usize]) -> &TreeNode {
        let mut n = &self.tree[path[0]];
        for &i in &path[1..] {
            n = &n.children[i];
        }
        n
    }

    fn find(&mut self, key: &NodeKey) -> Option<&mut TreeNode> {
        find_in(&mut self.tree, key)
    }

    fn selected_row(&self) -> Option<Vec<usize>> {
        let rows = self.rows();
        self.tree_state
            .selected()
            .and_then(|i| rows.get(i))
            .map(|r| r.path.clone())
    }

    fn selected_key(&self) -> Option<NodeKey> {
        self.selected_row().map(|p| self.node_at(&p).key.clone())
    }

    fn select_tree(&mut self, idx: usize) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        let idx = idx.min(rows.len() - 1);
        self.tree_state.select(Some(idx));
        self.tree_key = Some(self.node_at(&rows[idx].path).key.clone());
    }

    /// Dopo modifiche all'albero, ri-aggancia la selezione alla chiave salvata.
    fn sync_tree_sel(&mut self) {
        let rows = self.rows();
        if let Some(k) = &self.tree_key
            && let Some(i) = rows.iter().position(|r| &self.node_at(&r.path).key == k)
        {
            self.tree_state.select(Some(i));
            return;
        }
        let i = self.tree_state.selected().unwrap_or(0);
        self.select_tree(i);
    }

    /// Comando di caricamento lazy per un nodo espandibile.
    fn load_cmd(&self, key: &NodeKey) -> Option<Command> {
        match key {
            NodeKey::SubsRoot => Some(Command::DiscoverCli),
            NodeKey::Sub(id) => self.subs.get(id).cloned().map(Command::LoadAccounts),
            NodeKey::Blobs(id) => self.accounts.get(id).cloned().map(Command::ListContainers),
            NodeKey::Queues(id) => self.accounts.get(id).cloned().map(Command::ListQueues),
            _ => None,
        }
    }

    fn reload_node(&mut self, key: &NodeKey) -> Vec<Command> {
        let Some(cmd) = self.load_cmd(key) else {
            return vec![];
        };
        if *key == NodeKey::SubsRoot {
            self.cli = CliState::Checking;
        }
        if let Some(n) = self.find(key) {
            n.set_loading();
        }
        self.sync_tree_sel();
        vec![cmd]
    }

    fn toggle_node(&mut self, path: &[usize], expand: Option<bool>) -> Vec<Command> {
        let key = self.node_at(path).key.clone();
        let node = self.find(&key).unwrap();
        if node.leaf {
            return vec![];
        }
        let want = expand.unwrap_or(!node.expanded);
        node.expanded = want;
        if want && matches!(node.load, Load::Idle) {
            return self.reload_node(&key);
        }
        vec![]
    }

    fn tree_activate(&mut self) -> Vec<Command> {
        let Some(path) = self.selected_row() else {
            return vec![];
        };
        let key = self.node_at(&path).key.clone();
        match key {
            NodeKey::Container(acct, c) => self.open_container(acct, c, String::new()),
            NodeKey::Queue(acct, q) => self.open_queue(acct, q),
            _ => self.toggle_node(&path, None),
        }
    }

    fn open_container(
        &mut self,
        account_id: String,
        container: String,
        prefix: String,
    ) -> Vec<Command> {
        let Some(account) = self.accounts.get(&account_id).cloned() else {
            return vec![];
        };
        self.main = MainView::Blobs {
            account_id,
            container: container.clone(),
            prefix: prefix.clone(),
            entries: Vec::new(),
            loading: true,
            error: None,
        };
        self.clear_filter();
        self.main_state.select(None);
        self.reset_preview();
        self.panel = ActivePanel::MainItemList;
        vec![Command::ListBlobs {
            account,
            container,
            prefix,
        }]
    }

    fn open_queue(&mut self, account_id: String, queue: String) -> Vec<Command> {
        let Some(account) = self.accounts.get(&account_id).cloned() else {
            return vec![];
        };
        self.main = MainView::Messages {
            account_id,
            queue: queue.clone(),
            messages: Vec::new(),
            loading: true,
            error: None,
        };
        self.clear_filter();
        self.main_state.select(None);
        self.reset_preview();
        self.panel = ActivePanel::MainItemList;
        vec![Command::PeekMessages { account, queue }]
    }

    fn reset_preview(&mut self) {
        self.preview_key = None;
        self.preview = None;
        self.preview_scroll = 0;
    }

    /// Totale elementi caricati (senza filtro).
    pub fn main_total(&self) -> usize {
        match &self.main {
            MainView::Empty => 0,
            MainView::Blobs { entries, .. } => entries.len(),
            MainView::Messages { messages, .. } => messages.len(),
        }
    }

    /// Indici (in `entries`/`messages`) visibili dopo filtro e ordinamento.
    pub fn visible(&self) -> Vec<usize> {
        let terms: Vec<String> = self
            .filter
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let matches = |s: &str| {
            let s = s.to_lowercase();
            terms.iter().all(|t| s.contains(t.as_str()))
        };
        match &self.main {
            MainView::Empty => vec![],
            MainView::Blobs {
                prefix, entries, ..
            } => {
                let mut idx: Vec<usize> = entries
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| match e {
                        Entry::Parent => true,
                        _ if terms.is_empty() => true,
                        Entry::Dir(d) => matches(rel_name(d, prefix)),
                        Entry::Blob(b) => matches(rel_name(&b.name, prefix)),
                    })
                    .map(|(i, _)| i)
                    .collect();
                let sort = self.sort;
                idx.sort_by(|&a, &b| cmp_entries(&entries[a], &entries[b], sort));
                idx
            }
            MainView::Messages { messages, .. } => messages
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    terms.is_empty()
                        || matches(&m.id)
                        || matches(&m.text)
                        || matches(&decode_message(&m.text).0)
                })
                .map(|(i, _)| i)
                .collect(),
        }
    }

    pub fn main_len(&self) -> usize {
        if self.filter.trim().is_empty() {
            self.main_total()
        } else {
            self.visible().len()
        }
    }

    fn selected_index(&self) -> Option<usize> {
        let i = self.main_state.selected()?;
        self.visible().get(i).copied()
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        match &self.main {
            MainView::Blobs { entries, .. } => self.selected_index().and_then(|i| entries.get(i)),
            _ => None,
        }
    }

    pub fn selected_message(&self) -> Option<&QueueMessage> {
        match &self.main {
            MainView::Messages { messages, .. } => {
                self.selected_index().and_then(|i| messages.get(i))
            }
            _ => None,
        }
    }

    /// Chiave cache della cartella selezionata nella lista centrale.
    pub fn selected_dir_key(&self) -> Option<String> {
        let MainView::Blobs {
            account_id,
            container,
            ..
        } = &self.main
        else {
            return None;
        };
        match self.selected_entry() {
            Some(Entry::Dir(d)) => Some(dir_key(account_id, container, d)),
            _ => None,
        }
    }

    /// Chiamato periodicamente: carica (con debounce) la preview della cartella selezionata.
    pub fn tick(&mut self) -> Vec<Command> {
        let Some(key) = self.selected_dir_key() else {
            self.dir_sel = None;
            return vec![];
        };
        if self.dir_cache.contains_key(&key) {
            return vec![];
        }
        match &self.dir_sel {
            Some((k, t)) if *k == key => {
                if t.elapsed() < DIR_PREVIEW_DELAY {
                    return vec![];
                }
            }
            _ => {
                self.dir_sel = Some((key, Instant::now()));
                return vec![];
            }
        }
        let MainView::Blobs {
            account_id,
            container,
            ..
        } = &self.main
        else {
            return vec![];
        };
        let Some(Entry::Dir(prefix)) = self.selected_entry().cloned() else {
            return vec![];
        };
        let Some(account) = self.accounts.get(account_id).cloned() else {
            return vec![];
        };
        let container = container.clone();
        if self.dir_cache.len() >= DIR_CACHE_MAX {
            self.dir_cache
                .retain(|_, v| matches!(v, DirPreview::Loading));
        }
        self.dir_cache.insert(key, DirPreview::Loading);
        vec![Command::ListBlobs {
            account,
            container,
            prefix,
        }]
    }

    fn clear_filter(&mut self) {
        self.filter.clear();
        self.filter_editing = false;
    }

    /// Dopo una modifica del filtro: seleziona il primo risultato (saltando "..").
    fn on_filter_changed(&mut self) {
        let v = self.visible();
        let skip_parent = matches!(
            (&self.main, v.first()),
            (MainView::Blobs { entries, .. }, Some(&i)) if matches!(entries[i], Entry::Parent)
        ) && v.len() > 1
            && !self.filter.trim().is_empty();
        self.main_state
            .select((!v.is_empty()).then_some(skip_parent as usize));
        *self.main_state.offset_mut() = 0;
        self.preview_scroll = 0;
    }

    fn clamp_main_sel(&mut self) {
        let len = self.main_len();
        let sel = self.main_state.selected().unwrap_or(0);
        self.main_state.select((len > 0).then(|| sel.min(len - 1)));
    }

    fn cycle_sort(&mut self) {
        if !matches!(self.main, MainView::Blobs { .. }) {
            self.set_status("Sorting is available for blob listings");
            return;
        }
        let cur = self.selected_index();
        self.sort = self.sort.next();
        let pos = cur.and_then(|c| self.visible().iter().position(|&i| i == c));
        self.main_state.select(pos.or(self.main_state.selected()));
        self.clamp_main_sel();
        self.set_status(format!("Sort: {}", self.sort.label()));
    }

    fn start_filter(&mut self) {
        if matches!(self.main, MainView::Empty) {
            self.set_status("Open a container or queue first");
            return;
        }
        self.panel = ActivePanel::MainItemList;
        self.filter_editing = true;
    }

    fn key_filter(&mut self, k: KeyEvent) -> Vec<Command> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => {
                self.clear_filter();
                self.on_filter_changed();
            }
            KeyCode::Enter => {
                self.filter_editing = false;
                if self.filter.trim().is_empty() {
                    self.filter.clear();
                }
            }
            KeyCode::Up => self.move_main(-1),
            KeyCode::Down => self.move_main(1),
            KeyCode::PageUp => self.move_main(-15),
            KeyCode::PageDown => self.move_main(15),
            KeyCode::Backspace => {
                self.filter.pop();
                self.on_filter_changed();
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.on_filter_changed();
            }
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.on_filter_changed();
            }
            _ => {}
        }
        vec![]
    }

    fn refresh_main(&mut self) -> Vec<Command> {
        self.dir_cache.clear();
        let filter = self.filter.clone();
        let cmds = self.refresh_main_inner();
        self.filter = filter;
        cmds
    }

    fn refresh_main_inner(&mut self) -> Vec<Command> {
        match &self.main {
            MainView::Blobs {
                account_id,
                container,
                prefix,
                ..
            } => {
                let (a, c, p) = (account_id.clone(), container.clone(), prefix.clone());
                let sel = self.main_state.selected();
                let cmds = self.open_container(a, c, p);
                self.main_state.select(sel);
                cmds
            }
            MainView::Messages {
                account_id, queue, ..
            } => {
                let (a, q) = (account_id.clone(), queue.clone());
                let sel = self.main_state.selected();
                let cmds = self.open_queue(a, q);
                self.main_state.select(sel);
                cmds
            }
            MainView::Empty => vec![],
        }
    }

    fn main_activate(&mut self) -> Vec<Command> {
        let MainView::Blobs {
            account_id,
            container,
            prefix,
            ..
        } = &self.main
        else {
            if self.selected_message().is_some() {
                self.panel = ActivePanel::PreviewInspector;
            }
            return vec![];
        };
        let (a, c, p) = (account_id.clone(), container.clone(), prefix.clone());
        match self.selected_entry().cloned() {
            Some(Entry::Parent) => self.open_container(a, c, parent_prefix(&p)),
            Some(Entry::Dir(d)) => self.open_container(a, c, d),
            Some(Entry::Blob(b)) => {
                let Some(account) = self.accounts.get(&a).cloned() else {
                    return vec![];
                };
                self.preview_key = Some(format!("{a}/{c}/{}", b.name));
                self.preview = Some(PreviewState::Loading);
                self.preview_scroll = 0;
                self.panel = ActivePanel::PreviewInspector;
                vec![Command::PreviewBlob {
                    account,
                    container: c,
                    blob: b.name,
                }]
            }
            None => vec![],
        }
    }

    fn main_up_dir(&mut self) -> Vec<Command> {
        if let MainView::Blobs {
            account_id,
            container,
            prefix,
            ..
        } = &self.main
            && !prefix.is_empty()
        {
            let (a, c, p) = (account_id.clone(), container.clone(), parent_prefix(prefix));
            return self.open_container(a, c, p);
        }
        self.panel = ActivePanel::SidebarTree;
        vec![]
    }

    // ------------------------------ Delete ---------------------------------

    fn request_delete(&mut self) {
        let action = match self.panel {
            ActivePanel::SidebarTree => match self.selected_key() {
                Some(NodeKey::Account(id)) => match self.accounts.get(&id).map(|a| &a.source) {
                    Some(AccountSource::AttachedManual { profile_name }) => Some((
                        format!("Detach attached account '{profile_name}'?"),
                        PendingAction::DetachAccount {
                            profile: profile_name.clone(),
                        },
                    )),
                    _ => None,
                },
                _ => None,
            },
            _ => match &self.main {
                MainView::Blobs {
                    account_id,
                    container,
                    prefix,
                    ..
                } => match self.selected_entry() {
                    Some(Entry::Blob(b)) => Some((
                        format!("Delete blob '{container}/{}'?", b.name),
                        PendingAction::DeleteBlob {
                            account_id: account_id.clone(),
                            container: container.clone(),
                            blob: b.name.clone(),
                            prefix: prefix.clone(),
                        },
                    )),
                    _ => None,
                },
                MainView::Messages {
                    account_id, queue, ..
                } => self.selected_message().map(|m| {
                    (
                        format!("Delete message '{}' from '{queue}'?", m.id),
                        PendingAction::DeleteMessage {
                            account_id: account_id.clone(),
                            queue: queue.clone(),
                            id: m.id.clone(),
                        },
                    )
                }),
                MainView::Empty => None,
            },
        };
        match action {
            Some((target, action)) => {
                self.modal = ModalState::ConfirmDeleteDialog { target, action }
            }
            None => self.set_status("Nothing deletable selected"),
        }
    }

    fn request_clear(&mut self) {
        let target = match (self.panel, &self.main) {
            (ActivePanel::SidebarTree, _) => match self.selected_key() {
                Some(NodeKey::Queue(a, q)) => Some((a, q)),
                _ => None,
            },
            (
                _,
                MainView::Messages {
                    account_id, queue, ..
                },
            ) => Some((account_id.clone(), queue.clone())),
            _ => None,
        };
        match target {
            Some((account_id, queue)) => {
                self.modal = ModalState::ConfirmDeleteDialog {
                    target: format!("Clear ALL messages in queue '{queue}'?"),
                    action: PendingAction::ClearQueue { account_id, queue },
                }
            }
            None => self.set_status("Select a queue to clear"),
        }
    }

    // ------------------------------ Transfers ------------------------------

    fn request_download(&mut self) {
        let target = match (self.panel, &self.main) {
            (ActivePanel::SidebarTree, _) => match self.selected_key() {
                Some(NodeKey::Container(a, c)) => Some((a, c, DownloadSpec::Prefix(String::new()))),
                _ => None,
            },
            (
                _,
                MainView::Blobs {
                    account_id,
                    container,
                    prefix,
                    ..
                },
            ) => {
                let spec = match self.selected_entry() {
                    Some(Entry::Blob(b)) => DownloadSpec::Blob(b.name.clone()),
                    Some(Entry::Dir(d)) => DownloadSpec::Prefix(d.clone()),
                    _ => DownloadSpec::Prefix(prefix.clone()),
                };
                Some((account_id.clone(), container.clone(), spec))
            }
            _ => None,
        };
        let Some((account_id, container, spec)) = target else {
            self.set_status("Select a blob, folder or container to download");
            return;
        };
        let base = match &spec {
            DownloadSpec::Blob(n) => last_segment(n),
            DownloadSpec::Prefix(p) if p.is_empty() => container.clone(),
            DownloadSpec::Prefix(p) => last_segment(p),
        };
        let path = abbreviate(&downloads_dir().join(base));
        self.modal = ModalState::TransferDialog(TransferForm {
            kind: TransferKind::Download {
                account_id,
                container,
                spec,
            },
            path,
            overwrite: false,
            error: None,
        });
    }

    fn request_upload(&mut self) {
        let target = match (self.panel, &self.main) {
            (ActivePanel::SidebarTree, _) => match self.selected_key() {
                Some(NodeKey::Container(a, c)) => Some((a, c, String::new())),
                _ => None,
            },
            (
                _,
                MainView::Blobs {
                    account_id,
                    container,
                    prefix,
                    ..
                },
            ) => Some((account_id.clone(), container.clone(), prefix.clone())),
            _ => None,
        };
        let Some((account_id, container, prefix)) = target else {
            self.set_status("Open a container (or select one in the tree) to upload");
            return;
        };
        self.modal = ModalState::TransferDialog(TransferForm {
            kind: TransferKind::Upload {
                account_id,
                container,
                prefix,
            },
            path: String::new(),
            overwrite: false,
            error: None,
        });
    }

    fn submit_transfer(&mut self) -> Result<Vec<Command>, String> {
        let ModalState::TransferDialog(f) = &self.modal else {
            return Ok(vec![]);
        };
        if f.path.trim().is_empty() {
            return Err("Path is required".into());
        }
        let path = expand_tilde(f.path.trim());
        let overwrite = f.overwrite;
        match f.kind.clone() {
            TransferKind::Download {
                account_id,
                container,
                spec,
            } => {
                let account = self
                    .accounts
                    .get(&account_id)
                    .cloned()
                    .ok_or("account not found")?;
                let dest = match &spec {
                    DownloadSpec::Blob(name) => {
                        let dest = if path.is_dir() {
                            path.join(last_segment(name))
                        } else {
                            path
                        };
                        if dest.exists() && !overwrite {
                            return Err(format!(
                                "{} already exists (Ctrl-O to overwrite)",
                                abbreviate(&dest)
                            ));
                        }
                        dest
                    }
                    DownloadSpec::Prefix(_) => {
                        if path.is_file() {
                            return Err("Destination must be a folder".into());
                        }
                        path
                    }
                };
                self.set_status(format!("Downloading to {}…", abbreviate(&dest)));
                Ok(vec![Command::Download {
                    account,
                    container,
                    spec,
                    dest,
                    overwrite,
                }])
            }
            TransferKind::Upload {
                account_id,
                container,
                prefix,
            } => {
                let account = self
                    .accounts
                    .get(&account_id)
                    .cloned()
                    .ok_or("account not found")?;
                if !path.exists() {
                    return Err(format!("{} does not exist", abbreviate(&path)));
                }
                self.set_status(format!("Uploading {}…", abbreviate(&path)));
                Ok(vec![Command::Upload {
                    account,
                    container,
                    prefix,
                    src: path,
                    overwrite,
                }])
            }
        }
    }

    /// Ricarica la vista blob aperta senza cambiare focus/preview.
    fn reload_blobs_quiet(&mut self, acct: &str, cont: &str) -> Vec<Command> {
        let MainView::Blobs {
            account_id,
            container,
            prefix,
            loading,
            ..
        } = &mut self.main
        else {
            return vec![];
        };
        if account_id != acct || container != cont {
            return vec![];
        }
        let Some(account) = self.accounts.get(acct).cloned() else {
            return vec![];
        };
        let pfx = format!("{acct}/{cont}/");
        self.dir_cache.retain(|k, _| !k.starts_with(&pfx));
        *loading = true;
        vec![Command::ListBlobs {
            account,
            container: container.clone(),
            prefix: prefix.clone(),
        }]
    }

    fn execute(&mut self, action: PendingAction) -> Vec<Command> {
        let acct = |id: &str| self.accounts.get(id).cloned();
        match action {
            PendingAction::DeleteBlob {
                account_id,
                container,
                blob,
                prefix,
            } => acct(&account_id)
                .map(|account| {
                    vec![Command::DeleteBlob {
                        account,
                        container,
                        blob,
                        prefix,
                    }]
                })
                .unwrap_or_default(),
            PendingAction::DeleteMessage {
                account_id,
                queue,
                id,
            } => acct(&account_id)
                .map(|account| vec![Command::DeleteMessage { account, queue, id }])
                .unwrap_or_default(),
            PendingAction::ClearQueue { account_id, queue } => acct(&account_id)
                .map(|account| vec![Command::ClearQueue { account, queue }])
                .unwrap_or_default(),
            PendingAction::DetachAccount { profile } => {
                self.detach(&profile);
                vec![]
            }
        }
    }

    fn detach(&mut self, profile: &str) {
        self.config.attached_accounts.retain(|a| a.name != profile);
        if let Err(e) = config::save(&self.config) {
            self.modal = ModalState::ErrorPopup(format!("Salvataggio config fallito: {e:#}"));
            return;
        }
        let id = format!("attached:{profile}");
        self.accounts.remove(&id);
        if let Some(root) = self.find(&NodeKey::AttachedRoot) {
            root.children
                .retain(|c| c.key != NodeKey::Account(id.clone()));
            if root.children.is_empty() {
                root.children
                    .push(TreeNode::info("(none — press [a] to attach)"));
            }
        }
        let open = match &self.main {
            MainView::Blobs { account_id, .. } | MainView::Messages { account_id, .. } => {
                account_id == &id
            }
            MainView::Empty => false,
        };
        if open {
            self.main = MainView::Empty;
            self.reset_preview();
        }
        self.sync_tree_sel();
        self.set_status(format!("Detached '{profile}'"));
    }

    fn submit_add(&mut self) -> Result<(), String> {
        let ModalState::AddConnectionStringDialog(form) = &self.modal else {
            return Ok(());
        };
        let name = form.name.trim().to_string();
        let conn = form.conn.trim().to_string();
        if name.is_empty() {
            return Err("Name is required".into());
        }
        if self.config.attached_accounts.iter().any(|a| a.name == name) {
            return Err(format!("A profile named '{name}' already exists"));
        }
        auth::parse_connection_string(&name, &conn)
            .map_err(|e| format!("Invalid connection string / SAS URL: {e}"))?;
        let entry = AttachedAccount {
            name: name.clone(),
            connection_string: conn,
        };
        self.config.attached_accounts.push(entry.clone());
        if let Err(e) = config::save(&self.config) {
            self.config.attached_accounts.pop();
            return Err(format!("Saving config failed: {e:#}"));
        }
        let mut node = self.attached_node(&entry);
        node.expanded = true;
        if let Some(root) = self.find(&NodeKey::AttachedRoot) {
            root.children.retain(|c| !matches!(c.key, NodeKey::Info(_)));
            root.children.push(node);
            root.expanded = true;
        }
        self.tree_key = Some(NodeKey::Account(format!("attached:{name}")));
        self.sync_tree_sel();
        self.panel = ActivePanel::SidebarTree;
        self.set_status(format!("Attached '{name}'"));
        Ok(())
    }

    // ------------------------------ Input ----------------------------------

    pub fn on_paste(&mut self, text: String) {
        if self.filter_editing && matches!(self.modal, ModalState::None) {
            self.filter
                .extend(text.chars().filter(|c| !c.is_control()));
            self.on_filter_changed();
            return;
        }
        if let ModalState::TransferDialog(f) = &mut self.modal {
            // Drag & drop da Finder incolla il path (con escape/virgolette).
            f.path = clean_dropped_path(&text);
            f.error = None;
            return;
        }
        if let ModalState::AddConnectionStringDialog(f) = &mut self.modal {
            let clean: String = text.chars().filter(|c| !c.is_control()).collect();
            if f.field == 0 {
                f.name.push_str(&clean)
            } else {
                f.conn.push_str(&clean)
            }
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Vec<Command> {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return vec![];
        }
        match &mut self.modal {
            ModalState::None => {}
            ModalState::ErrorPopup(_) => {
                if matches!(
                    k.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ')
                ) {
                    self.modal = ModalState::None;
                }
                return vec![];
            }
            ModalState::ConfirmDeleteDialog { action, .. } => {
                let action = action.clone();
                return match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        self.modal = ModalState::None;
                        self.execute(action)
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc | KeyCode::Enter => {
                        self.modal = ModalState::None;
                        vec![]
                    }
                    _ => vec![],
                };
            }
            ModalState::TransferDialog(f) => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Esc => self.modal = ModalState::None,
                    KeyCode::Enter => match self.submit_transfer() {
                        Ok(cmds) => {
                            self.modal = ModalState::None;
                            return cmds;
                        }
                        Err(e) => {
                            if let ModalState::TransferDialog(f) = &mut self.modal {
                                f.error = Some(e);
                            }
                        }
                    },
                    KeyCode::Char('o') if ctrl => f.overwrite = !f.overwrite,
                    KeyCode::Char('u') if ctrl => f.path.clear(),
                    KeyCode::Backspace => {
                        f.path.pop();
                    }
                    KeyCode::Char(c) if !ctrl => f.path.push(c),
                    _ => {}
                }
                return vec![];
            }
            ModalState::AddConnectionStringDialog(f) => {
                match k.code {
                    KeyCode::Esc => self.modal = ModalState::None,
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                        f.field = 1 - f.field
                    }
                    KeyCode::Enter if f.field == 0 => f.field = 1,
                    KeyCode::Enter => match self.submit_add() {
                        Ok(()) => self.modal = ModalState::None,
                        Err(e) => {
                            if let ModalState::AddConnectionStringDialog(f) = &mut self.modal {
                                f.error = Some(e);
                            }
                        }
                    },
                    KeyCode::Backspace => {
                        if f.field == 0 {
                            f.name.pop();
                        } else {
                            f.conn.pop();
                        }
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        if f.field == 0 {
                            f.name.clear()
                        } else {
                            f.conn.clear()
                        }
                    }
                    KeyCode::Char(c) => {
                        if f.field == 0 {
                            f.name.push(c)
                        } else {
                            f.conn.push(c)
                        }
                    }
                    _ => {}
                }
                return vec![];
            }
        }

        if self.filter_editing {
            return self.key_filter(k);
        }

        // Tasti globali
        match k.code {
            KeyCode::Char('/') => {
                self.start_filter();
                return vec![];
            }
            KeyCode::Char('q') => {
                self.should_quit = true;
                return vec![];
            }
            KeyCode::Tab => {
                self.panel = match self.panel {
                    ActivePanel::SidebarTree => ActivePanel::MainItemList,
                    ActivePanel::MainItemList => ActivePanel::PreviewInspector,
                    ActivePanel::PreviewInspector => ActivePanel::SidebarTree,
                };
                return vec![];
            }
            KeyCode::BackTab => {
                self.panel = match self.panel {
                    ActivePanel::SidebarTree => ActivePanel::PreviewInspector,
                    ActivePanel::MainItemList => ActivePanel::SidebarTree,
                    ActivePanel::PreviewInspector => ActivePanel::MainItemList,
                };
                return vec![];
            }
            KeyCode::Char('a') => {
                self.modal = ModalState::AddConnectionStringDialog(AddForm::default());
                return vec![];
            }
            KeyCode::Char('d') | KeyCode::Delete => {
                self.request_delete();
                return vec![];
            }
            KeyCode::Char('c') => {
                self.request_clear();
                return vec![];
            }
            KeyCode::Char('s') => {
                self.request_download();
                return vec![];
            }
            KeyCode::Char('u') => {
                self.request_upload();
                return vec![];
            }
            KeyCode::Char('x') if !self.transfers.is_empty() => {
                self.set_status("Cancelling transfers…");
                return vec![Command::CancelTransfers];
            }
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.set_status(if self.mouse_enabled {
                    "Mouse capture ON"
                } else {
                    "Mouse capture OFF (native text selection enabled)"
                });
                return vec![];
            }
            _ => {}
        }

        match self.panel {
            ActivePanel::SidebarTree => self.key_tree(k),
            ActivePanel::MainItemList => self.key_main(k),
            ActivePanel::PreviewInspector => self.key_preview(k),
        }
    }

    // ------------------------------ Mouse ----------------------------------

    pub fn on_mouse(&mut self, m: MouseEvent) -> Vec<Command> {
        let pos = Position::new(m.column, m.row);
        let left_down = matches!(m.kind, MouseEventKind::Down(MouseButton::Left));
        match &mut self.modal {
            ModalState::None => {}
            ModalState::ErrorPopup(_) => {
                if left_down {
                    self.modal = ModalState::None;
                }
                return vec![];
            }
            ModalState::ConfirmDeleteDialog { action, .. } => {
                if !left_down {
                    return vec![];
                }
                if self.hit.confirm_yes.contains(pos) {
                    let action = action.clone();
                    self.modal = ModalState::None;
                    return self.execute(action);
                }
                if self.hit.confirm_no.contains(pos) || !self.hit.modal.contains(pos) {
                    self.modal = ModalState::None;
                }
                return vec![];
            }
            ModalState::TransferDialog(_) => return vec![],
            ModalState::AddConnectionStringDialog(f) => {
                // Click esterni ignorati per non perdere l'input digitato.
                if left_down
                    && let Some(i) = self.hit.form_fields.iter().position(|r| r.contains(pos))
                {
                    f.field = i;
                }
                return vec![];
            }
        }

        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.click(pos),
            MouseEventKind::ScrollUp => {
                self.wheel(pos, -3);
                vec![]
            }
            MouseEventKind::ScrollDown => {
                self.wheel(pos, 3);
                vec![]
            }
            _ => vec![],
        }
    }

    fn is_double(&mut self, panel: ActivePanel, idx: usize) -> bool {
        let now = Instant::now();
        let double = self.last_click.is_some_and(|(t, p, i)| {
            p == panel && i == idx && now.duration_since(t) < DOUBLE_CLICK
        });
        self.last_click = if double {
            None
        } else {
            Some((now, panel, idx))
        };
        double
    }

    fn click(&mut self, pos: Position) -> Vec<Command> {
        let h = self.hit;
        if h.tree.contains(pos) {
            self.panel = ActivePanel::SidebarTree;
            if !h.tree_list.contains(pos) {
                return vec![];
            }
            let idx = self.tree_state.offset() + (pos.y - h.tree_list.y) as usize;
            let rows = self.rows();
            let Some(row) = rows.get(idx) else {
                return vec![];
            };
            let path = row.path.clone();
            let arrow_x = h.tree_list.x + (row.depth * 2) as u16;
            self.select_tree(idx);
            let double = self.is_double(ActivePanel::SidebarTree, idx);
            // Click sulla freccia ▸/▾: espandi/comprimi.
            if !self.node_at(&path).leaf && (arrow_x..arrow_x + 2).contains(&pos.x) {
                self.last_click = None;
                return self.toggle_node(&path, None);
            }
            if double {
                return self.tree_activate();
            }
        } else if h.main.contains(pos) {
            self.panel = ActivePanel::MainItemList;
            if !h.main_list.contains(pos) {
                return vec![];
            }
            let idx = self.main_state.offset() + (pos.y - h.main_list.y) as usize;
            if idx >= self.main_len() {
                return vec![];
            }
            if self.main_state.selected() != Some(idx) {
                self.main_state.select(Some(idx));
                self.preview_scroll = 0;
            }
            if self.is_double(ActivePanel::MainItemList, idx) {
                return self.main_activate();
            }
        } else if h.preview.contains(pos) {
            self.panel = ActivePanel::PreviewInspector;
        }
        vec![]
    }

    fn wheel(&mut self, pos: Position, delta: isize) {
        let h = self.hit;
        if h.tree.contains(pos) {
            let cur = self.tree_state.selected().unwrap_or(0) as isize;
            self.select_tree((cur + delta).max(0) as usize);
        } else if h.main.contains(pos) {
            self.move_main(delta);
        } else if h.preview.contains(pos) {
            self.preview_scroll = if delta < 0 {
                self.preview_scroll
                    .saturating_sub(delta.unsigned_abs() as u16)
            } else {
                self.preview_scroll.saturating_add(delta as u16)
            };
        }
    }

    fn key_tree(&mut self, k: KeyEvent) -> Vec<Command> {
        let len = self.rows().len();
        let cur = self.tree_state.selected().unwrap_or(0);
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_tree(cur.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select_tree(cur + 1),
            KeyCode::PageUp => self.select_tree(cur.saturating_sub(15)),
            KeyCode::PageDown => self.select_tree(cur + 15),
            KeyCode::Home | KeyCode::Char('g') => self.select_tree(0),
            KeyCode::End | KeyCode::Char('G') => self.select_tree(len.saturating_sub(1)),
            KeyCode::Enter | KeyCode::Char(' ') => return self.tree_activate(),
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(p) = self.selected_row() {
                    if self.node_at(&p).leaf {
                        return self.tree_activate();
                    }
                    return self.toggle_node(&p, Some(true));
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(p) = self.selected_row() {
                    let n = self.node_at(&p);
                    if !n.leaf && n.expanded {
                        return self.toggle_node(&p, Some(false));
                    }
                    if p.len() > 1 {
                        let parent = &p[..p.len() - 1];
                        let rows = self.rows();
                        if let Some(i) = rows.iter().position(|r| r.path == parent) {
                            self.select_tree(i);
                        }
                    }
                }
            }
            KeyCode::Char('r') => {
                let Some(key) = self.selected_key() else {
                    return vec![];
                };
                return match key {
                    NodeKey::Container(a, _) => self.reload_node(&NodeKey::Blobs(a)),
                    NodeKey::Queue(a, _) => self.reload_node(&NodeKey::Queues(a)),
                    NodeKey::Account(a) => {
                        let mut cmds = Vec::new();
                        for k in [NodeKey::Blobs(a.clone()), NodeKey::Queues(a)] {
                            if self.find(&k).is_some_and(|n| n.load != Load::Idle) {
                                cmds.extend(self.reload_node(&k));
                            }
                        }
                        cmds
                    }
                    k => self.reload_node(&k),
                };
            }
            _ => {}
        }
        vec![]
    }

    fn move_main(&mut self, delta: isize) {
        let len = self.main_len();
        if len == 0 {
            return;
        }
        let cur = self.main_state.selected().map(|i| i as isize).unwrap_or(-1);
        let next = (cur + delta).clamp(0, len as isize - 1) as usize;
        if Some(next) != self.main_state.selected() {
            self.main_state.select(Some(next));
            self.preview_scroll = 0;
        }
    }

    fn key_main(&mut self, k: KeyEvent) -> Vec<Command> {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_main(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_main(1),
            KeyCode::PageUp => self.move_main(-15),
            KeyCode::PageDown => self.move_main(15),
            KeyCode::Home | KeyCode::Char('g') => self.move_main(-(self.main_len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_main(self.main_len() as isize),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => return self.main_activate(),
            KeyCode::Esc if !self.filter.is_empty() => {
                self.clear_filter();
                self.on_filter_changed();
            }
            KeyCode::Char('o') => self.cycle_sort(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc => {
                return self.main_up_dir();
            }
            KeyCode::Char('r') => return self.refresh_main(),
            _ => {}
        }
        vec![]
    }

    fn key_preview(&mut self, k: KeyEvent) -> Vec<Command> {
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.preview_scroll = self.preview_scroll.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.preview_scroll = self.preview_scroll.saturating_add(1)
            }
            KeyCode::PageUp => self.preview_scroll = self.preview_scroll.saturating_sub(20),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.preview_scroll = self.preview_scroll.saturating_add(20)
            }
            KeyCode::Home | KeyCode::Char('g') => self.preview_scroll = 0,
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Esc | KeyCode::Backspace => {
                self.panel = ActivePanel::MainItemList
            }
            KeyCode::Char('r') => return self.refresh_main(),
            _ => {}
        }
        vec![]
    }

    // ------------------------------ Backend --------------------------------

    pub fn on_backend(&mut self, ev: Event) -> Vec<Command> {
        let mut cmds = Vec::new();
        match ev {
            Event::CliUser(Ok(u)) => self.cli = CliState::Active(u),
            Event::CliUser(Err(e)) => {
                self.cli = CliState::Unavailable(e.clone());
                if let Some(n) = self.find(&NodeKey::SubsRoot) {
                    n.set_error(&format!("az login non attivo: {e}"));
                }
            }
            Event::Subscriptions(r) => {
                let nodes = r.as_ref().ok().map(|subs| {
                    subs.iter()
                        .map(|s| {
                            TreeNode::new(NodeKey::Sub(s.id.clone()), &s.name, false)
                                .with_detail(s.id.chars().take(8).collect::<String>())
                        })
                        .collect::<Vec<_>>()
                });
                if let Ok(subs) = &r {
                    self.subs = subs.iter().map(|s| (s.id.clone(), s.clone())).collect();
                }
                if let Some(n) = self.find(&NodeKey::SubsRoot) {
                    match (&r, nodes) {
                        (_, Some(nodes)) => n.set_children(nodes),
                        (Err(e), _) => n.set_error(e),
                        _ => {}
                    }
                }
                // Con una sola subscription la si espande automaticamente.
                if let Ok(subs) = &r
                    && subs.len() == 1
                {
                    let key = NodeKey::Sub(subs[0].id.clone());
                    cmds.extend(self.reload_node(&key));
                }
            }
            Event::Accounts { sub_id, result } => {
                let nodes = match &result {
                    Ok(list) => {
                        for a in list {
                            let mut a = a.clone();
                            if let Some(old) = self.accounts.get(&a.id) {
                                a.credential = old.credential.clone();
                            }
                            self.accounts.insert(a.id.clone(), a);
                        }
                        Ok(list.iter().map(account_node).collect::<Vec<_>>())
                    }
                    Err(e) => Err(e.clone()),
                };
                if let Some(n) = self.find(&NodeKey::Sub(sub_id)) {
                    match nodes {
                        Ok(v) => n.set_children(v),
                        Err(e) => n.set_error(&e),
                    }
                }
            }
            Event::Credential { account_id, cred } => {
                if let Some(a) = self.accounts.get_mut(&account_id) {
                    let msg = format!("{}: auth via {}", a.name, cred.label());
                    a.credential = Some(cred);
                    self.set_status(msg);
                }
            }
            Event::Containers { account_id, result } => {
                let key = NodeKey::Blobs(account_id.clone());
                if let Some(n) = self.find(&key) {
                    match result {
                        Ok(list) => n.set_children(
                            list.into_iter()
                                .map(|c| {
                                    TreeNode::new(
                                        NodeKey::Container(account_id.clone(), c.clone()),
                                        c,
                                        true,
                                    )
                                })
                                .collect(),
                        ),
                        Err(e) => n.set_error(&e),
                    }
                }
            }
            Event::Queues { account_id, result } => {
                let key = NodeKey::Queues(account_id.clone());
                if let Some(n) = self.find(&key) {
                    match result {
                        Ok(list) => n.set_children(
                            list.into_iter()
                                .map(|q| {
                                    let detail = q
                                        .approx_count
                                        .map(|c| format!("({c} msgs)"))
                                        .unwrap_or_default();
                                    TreeNode::new(
                                        NodeKey::Queue(account_id.clone(), q.name.clone()),
                                        q.name,
                                        true,
                                    )
                                    .with_detail(detail)
                                })
                                .collect(),
                        ),
                        Err(e) => n.set_error(&e),
                    }
                }
            }
            Event::Blobs {
                account_id,
                container,
                prefix,
                result,
            } => {
                let key = dir_key(&account_id, &container, &prefix);
                if let Some(slot) = self.dir_cache.get_mut(&key) {
                    *slot = match &result {
                        Ok(l) => DirPreview::Ready(build_entries(&prefix, l.clone())),
                        Err(e) => DirPreview::Failed(e.clone()),
                    };
                }
                if let MainView::Blobs {
                    account_id: a,
                    container: c,
                    prefix: p,
                    entries,
                    loading,
                    error,
                } = &mut self.main
                    && *a == account_id
                    && *c == container
                    && *p == prefix
                {
                    *loading = false;
                    match result {
                        Ok(l) => {
                            *error = None;
                            *entries = build_entries(&prefix, l);
                        }
                        Err(e) => {
                            *error = Some(e);
                            entries.clear();
                            if !prefix.is_empty() {
                                entries.push(Entry::Parent);
                            }
                        }
                    }
                    self.clamp_main_sel();
                }
            }
            Event::BlobPreview {
                account_id,
                container,
                blob,
                result,
            } => {
                if self.preview_key.as_deref() == Some(&format!("{account_id}/{container}/{blob}"))
                {
                    self.preview = Some(match result {
                        Ok(p) => PreviewState::Ready {
                            text: render_bytes(&blob, &p.data),
                            truncated: p.truncated,
                        },
                        Err(e) => PreviewState::Failed(e),
                    });
                }
            }
            Event::Messages {
                account_id,
                queue,
                result,
            } => {
                if let MainView::Messages {
                    account_id: a,
                    queue: q,
                    messages,
                    loading,
                    error,
                } = &mut self.main
                    && *a == account_id
                    && *q == queue
                {
                    *loading = false;
                    match result {
                        Ok(m) => {
                            *error = None;
                            *messages = m;
                        }
                        Err(e) => {
                            *error = Some(e);
                            messages.clear();
                        }
                    }
                    self.clamp_main_sel();
                    self.preview_scroll = 0;
                }
            }
            Event::Transfer { id, progress } => {
                self.transfers.insert(id, progress);
            }
            Event::TransferDone {
                id,
                account_id,
                container,
                upload,
                result,
            } => {
                self.transfers.remove(&id);
                match result {
                    Ok(msg) => self.set_status(msg),
                    Err(e) if e.contains("cancelled") => self.set_status("Transfer cancelled"),
                    Err(e) => {
                        self.modal = ModalState::ErrorPopup(format!(
                            "{} failed: {e}",
                            if upload { "Upload" } else { "Download" }
                        ))
                    }
                }
                if upload {
                    cmds.extend(self.reload_blobs_quiet(&account_id, &container));
                }
            }
            Event::Info(s) => self.set_status(s),
            Event::Error(e) => self.modal = ModalState::ErrorPopup(e),
        }
        self.sync_tree_sel();
        cmds
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn last_segment(p: &str) -> String {
    p.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(p)
        .to_string()
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn downloads_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|d| d.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| home().join("Downloads"))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    match p.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home().join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(p),
    }
}

pub fn abbreviate(p: &Path) -> String {
    match p.strip_prefix(home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => p.display().to_string(),
    }
}

/// Normalizza un path incollato/trascinato: virgolette, escape `\ `, file://.
pub fn clean_dropped_path(s: &str) -> String {
    let t = s.trim();
    let t = t
        .strip_prefix('\'')
        .and_then(|x| x.strip_suffix('\''))
        .or_else(|| t.strip_prefix('"').and_then(|x| x.strip_suffix('"')))
        .unwrap_or(t);
    if let Some(u) = t.strip_prefix("file://") {
        return urlencoding::decode(u)
            .map(|c| c.into_owned())
            .unwrap_or_else(|_| u.to_string());
    }
    let mut out = String::new();
    let mut chars = t.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn parent_prefix(p: &str) -> String {
    let t = p.trim_end_matches('/');
    match t.rfind('/') {
        Some(i) => t[..=i].to_string(),
        None => String::new(),
    }
}

fn dir_key(account_id: &str, container: &str, prefix: &str) -> String {
    format!("{account_id}/{container}/{prefix}")
}

/// Ordina le voci di una cartella secondo `sort` (cartelle prima).
pub fn sorted_entries(entries: &[Entry], sort: SortMode) -> Vec<&Entry> {
    let mut v: Vec<&Entry> = entries
        .iter()
        .filter(|e| !matches!(e, Entry::Parent))
        .collect();
    v.sort_by(|a, b| cmp_entries(a, b, sort));
    v
}

/// Nome relativo al prefisso corrente (quello mostrato in lista).
pub fn rel_name<'a>(name: &'a str, prefix: &str) -> &'a str {
    name.get(prefix.len()..)
        .filter(|_| name.starts_with(prefix))
        .unwrap_or(name)
}

fn parse_time(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc2822(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s))
        .ok()
}

fn cmp_entries(a: &Entry, b: &Entry, sort: SortMode) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |e: &Entry| match e {
        Entry::Parent => 0,
        Entry::Dir(_) => 1,
        Entry::Blob(_) => 2,
    };
    let name = |e: &Entry| match e {
        Entry::Parent => String::new(),
        Entry::Dir(d) => d.to_lowercase(),
        Entry::Blob(b) => b.name.to_lowercase(),
    };
    let by_name = || name(a).cmp(&name(b));
    rank(a).cmp(&rank(b)).then_with(|| match (sort, a, b) {
        (SortMode::NameDesc, ..) => by_name().reverse(),
        (SortMode::SizeDesc, Entry::Blob(x), Entry::Blob(y)) => {
            y.size.cmp(&x.size).then_with(by_name)
        }
        (SortMode::Newest, Entry::Blob(x), Entry::Blob(y)) => {
            match (parse_time(&x.last_modified), parse_time(&y.last_modified)) {
                (Some(tx), Some(ty)) => ty.cmp(&tx),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            }
            .then_with(by_name)
        }
        _ => by_name(),
    })
}

fn build_entries(prefix: &str, l: BlobListing) -> Vec<Entry> {
    let mut out = Vec::new();
    if !prefix.is_empty() {
        out.push(Entry::Parent);
    }
    out.extend(l.prefixes.into_iter().map(Entry::Dir));
    out.extend(
        l.blobs
            .into_iter()
            .filter(|b| b.name != prefix && !b.name.ends_with('/'))
            .map(Entry::Blob),
    );
    out
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
}

/// Testo (JSON formattato se possibile) o hex dump per contenuti binari.
pub fn render_bytes(_name: &str, data: &[u8]) -> String {
    if data.is_empty() {
        return "(empty blob)".into();
    }
    let text = match std::str::from_utf8(data) {
        Ok(s) => Some(s.to_string()),
        // Il troncamento a 512 KB può spezzare un carattere multibyte.
        Err(e) if e.valid_up_to() + 4 >= data.len() => {
            Some(String::from_utf8_lossy(&data[..e.valid_up_to()]).into_owned())
        }
        Err(_) => None,
    };
    match text {
        Some(t)
            if !t
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            pretty_json(&t)
                .unwrap_or(t)
                .replace('\t', "    ")
                .replace('\r', "")
        }
        _ => hexdump(&data[..data.len().min(4096)]),
    }
}

fn hexdump(data: &[u8]) -> String {
    let mut out = String::from("(binary content — hex dump of first 4 KB)\n\n");
    for (i, chunk) in data.chunks(16).enumerate() {
        let hex: Vec<String> = chunk.iter().map(|b| format!("{b:02x}")).collect();
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!("{:08x}  {:<48} {ascii}\n", i * 16, hex.join(" ")));
    }
    out
}

/// Decodifica il corpo di un messaggio di coda (spesso base64 dagli SDK).
pub fn decode_message(text: &str) -> (String, bool) {
    let t = text.trim();
    let looks_b64 = !t.is_empty()
        && t.len().is_multiple_of(4)
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='));
    if looks_b64
        && let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(t)
        && let Ok(s) = String::from_utf8(bytes)
        && !s
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return (pretty_json(&s).unwrap_or(s), true);
    }
    (pretty_json(text).unwrap_or_else(|| text.to_string()), false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_dropped_paths() {
        assert_eq!(
            clean_dropped_path("/Users/a/My\\ File.txt "),
            "/Users/a/My File.txt"
        );
        assert_eq!(clean_dropped_path("'/tmp/a b'"), "/tmp/a b");
        assert_eq!(clean_dropped_path("file:///tmp/a%20b"), "/tmp/a b");
    }

    #[test]
    fn filters_and_sorts_entries() {
        let blob = |n: &str, s: u64| {
            Entry::Blob(BlobItem {
                name: n.into(),
                size: s,
                content_type: String::new(),
                last_modified: String::new(),
                blob_type: String::new(),
                etag: String::new(),
            })
        };
        let (mut app, _) = App::new(Config::default(), Arc::new(AtomicUsize::new(0)));
        app.main = MainView::Blobs {
            account_id: "a".into(),
            container: "c".into(),
            prefix: "p/".into(),
            entries: vec![
                Entry::Parent,
                Entry::Dir("p/Logs/".into()),
                Entry::Dir("p/data/".into()),
                blob("p/small.log", 1),
                blob("p/big.json", 9),
            ],
            loading: false,
            error: None,
        };
        assert_eq!(app.visible(), vec![0, 2, 1, 4, 3]);
        app.sort = SortMode::SizeDesc;
        assert_eq!(app.visible(), vec![0, 2, 1, 4, 3]);
        app.sort = SortMode::NameDesc;
        assert_eq!(app.visible(), vec![0, 1, 2, 3, 4]);
        app.filter = "LOG".into();
        assert_eq!(app.visible(), vec![0, 1, 3]);
        app.filter = "p/".into(); // il prefisso non conta nel match
        assert_eq!(app.visible(), vec![0]);
    }

    #[test]
    fn parent_prefixes() {
        assert_eq!(parent_prefix("a/b/"), "a/");
        assert_eq!(parent_prefix("a/"), "");
        assert_eq!(parent_prefix(""), "");
    }

    #[test]
    fn decodes_base64_messages() {
        let (s, b) = decode_message("eyJhIjoxfQ==");
        assert!(b);
        assert!(s.contains("\"a\": 1"));
        let (s, b) = decode_message("hello world");
        assert!(!b);
        assert_eq!(s, "hello world");
    }
}
