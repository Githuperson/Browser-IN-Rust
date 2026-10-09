//! Bookmarks: a flat list of items plus folders, rendered into a tree for the UI.
//!
//! Stored flat (not nested JSON) so that moving a bookmark is a single field update
//! and so a corrupt subtree cannot make the whole file unreadable.

use crate::{store, time, Error, Persistent, ProfilePaths, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
  pub id: String,
  pub url: String,
  pub title: String,
  /// `None` means the bookmark lives at the root of the bookmarks bar.
  pub parent: Option<String>,
  pub added_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
  pub id: String,
  pub title: String,
  pub parent: Option<String>,
  pub added_at: u64,
}

/// A node in the tree handed to the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BookmarkNode {
  Folder {
    id: String,
    title: String,
    children: Vec<BookmarkNode>,
  },
  Item {
    id: String,
    url: String,
    title: String,
  },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct BookmarkDoc {
  folders: Vec<Folder>,
  items: Vec<Bookmark>,
  next_id: u64,
}

/// Note: the document is nested under `doc` rather than flattened, because
/// `#[serde(flatten)]` forces map-based serialization and makes error messages about
/// malformed files considerably worse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookmarkStore {
  doc: BookmarkDoc,
  #[serde(skip)]
  dirty: bool,
  #[serde(skip)]
  by_url: HashMap<String, String>,
}

impl BookmarkStore {
  pub fn load(paths: &ProfilePaths) -> Result<Self> {
    let path = paths.data_dir().join(Self::FILE);
    let mut store: Self = match store::read_to_string(&path)? {
      Some(doc) => serde_json::from_str(&doc).unwrap_or_default(),
      None => Self::default(),
    };
    store.reindex();
    Ok(store)
  }

  pub fn save_to(&self, paths: &ProfilePaths) -> Result<()> {
    let doc = serde_json::to_string_pretty(&self.doc)?;
    store::write_atomic(&paths.data_dir().join(Self::FILE), doc.as_bytes())
  }

  fn reindex(&mut self) {
    self.by_url.clear();
    for item in &self.doc.items {
      self.by_url.insert(item.url.clone(), item.id.clone());
    }
  }

  fn next_id(&mut self, prefix: &str) -> String {
    self.doc.next_id += 1;
    format!("{prefix}{}", self.doc.next_id)
  }

  pub fn add(&mut self, url: &str, title: &str, parent: Option<String>) -> String {
    if let Some(id) = self.by_url.get(url) {
      // Already bookmarked: refresh the title and leave it where it is.
      if let Some(item) = self.doc.items.iter_mut().find(|i| i.id == *id) {
        if !title.is_empty() {
          item.title = title.to_string();
        }
      }
      self.dirty = true;
      return id.clone();
    }
    let id = self.next_id("b");
    self.doc.items.push(Bookmark {
      id: id.clone(),
      url: url.to_string(),
      title: if title.is_empty() {
        url.to_string()
      } else {
        title.to_string()
      },
      parent,
      added_at: time::now_secs(),
    });
    self.by_url.insert(url.to_string(), id.clone());
    self.dirty = true;
    id
  }

  pub fn add_folder(&mut self, title: &str, parent: Option<String>) -> String {
    let id = self.next_id("f");
    self.doc.folders.push(Folder {
      id: id.clone(),
      title: title.to_string(),
      parent,
      added_at: time::now_secs(),
    });
    self.dirty = true;
    id
  }

  /// Remove an item or folder. Removing a folder removes its children recursively.
  pub fn remove(&mut self, id: &str) {
    let mut doomed = vec![id.to_string()];
    let mut removed_folders: Vec<String> = Vec::new();
    while let Some(current) = doomed.pop() {
      removed_folders.push(current.clone());
      for folder in &self.doc.folders {
        if folder.parent.as_deref() == Some(current.as_str()) {
          doomed.push(folder.id.clone());
        }
      }
      self.doc.items.retain(|i| i.parent.as_deref() != Some(current.as_str()));
    }
    self
      .doc
      .folders
      .retain(|f| !removed_folders.contains(&f.id));
    self.doc.items.retain(|i| i.id != id);
    self.reindex();
    self.dirty = true;
  }

  pub fn rename(&mut self, id: &str, title: &str) {
    if let Some(item) = self.doc.items.iter_mut().find(|i| i.id == id) {
      item.title = title.to_string();
      self.dirty = true;
    } else if let Some(folder) = self.doc.folders.iter_mut().find(|f| f.id == id) {
      folder.title = title.to_string();
      self.dirty = true;
    }
  }

  pub fn is_bookmarked(&self, url: &str) -> bool {
    self.by_url.contains_key(url)
  }

  /// Build the display tree. Iterative per level: bookmark trees are shallow, but a
  /// pathological file should not be able to blow the stack.
  pub fn tree(&self) -> Vec<BookmarkNode> {
    fn collect(store: &BookmarkStore, parent: Option<&str>) -> Vec<BookmarkNode> {
      let mut nodes: Vec<BookmarkNode> = Vec::new();
      for folder in &store.doc.folders {
        if folder.parent.as_deref() == parent {
          nodes.push(BookmarkNode::Folder {
            id: folder.id.clone(),
            title: folder.title.clone(),
            children: collect(store, Some(&folder.id)),
          });
        }
      }
      for item in &store.doc.items {
        if item.parent.as_deref() == parent {
          nodes.push(BookmarkNode::Item {
            id: item.id.clone(),
            url: item.url.clone(),
            title: item.title.clone(),
          });
        }
      }
      nodes
    }
    collect(self, None)
  }

  /// Flat list for the bookmarks bar: top-level items plus folders as single entries.
  pub fn bar(&self) -> Vec<BookmarkNode> {
    self.tree()
  }

  pub fn iter_items(&self) -> impl Iterator<Item = &Bookmark> {
    self.doc.items.iter()
  }
}

impl Persistent for BookmarkStore {
  const FILE: &'static str = "bookmarks.json";

  fn mark_dirty(&mut self) {
    self.dirty = true;
  }
  fn is_dirty(&self) -> bool {
    self.dirty
  }
  fn clear_dirty(&mut self) {
    self.dirty = false;
  }
  fn to_json(&self) -> Result<String> {
    serde_json::to_string_pretty(&self.doc).map_err(Error::from)
  }
}
