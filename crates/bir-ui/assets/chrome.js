/*
 * Chrome behaviour.
 *
 * Two channels:
 *   out: window.ipc.postMessage(JSON.stringify(command))
 *   in:  bir.onEvent(event)  — called by Rust
 *
 * Everything is rendered from a single `state` object, so a late-arriving event
 * (title, favicon, zoom) re-renders the whole strip correctly instead of patching one
 * node that may no longer exist.
 */
(function () {
  'use strict';

  var state = {
    tabs: [],
    active: null,
    settings: null,
    theme: 'system',
    extensions: [],
    history: [],
    bookmarks: [],
    downloads: [],
    suggestions: [],
    selected: -1,
    requestId: 0,
    findOpen: false,
    findText: '',
    stats: null,
    prompt: null
  };

  var el = {};
  var IDLE = 0;

  function $(id) { return document.getElementById(id); }
  function send(command) { window.ipc.postMessage(JSON.stringify(command)); }
  function activeTab() {
    for (var i = 0; i < state.tabs.length; i++) {
      if (state.tabs[i].id === state.active) { return state.tabs[i]; }
    }
    return null;
  }
  function escapeHtml(text) {
    return String(text === null || text === undefined ? '' : text)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }
  function hostOf(url) {
    try { return new URL(url).host; } catch (e) {
      var match = /^(?:[a-z]+:)?\/\/([^/]+)/i.exec(url || '');
      return match ? match[1] : (url || '');
    }
  }

  // ---------------------------------------------------------------- rendering

  function render() {
    renderTabs();
    renderToolbar();
    renderExtensions();
  }

  function renderTabs() {
    var strip = el.tabs;
    strip.textContent = '';
    for (var i = 0; i < state.tabs.length; i++) {
      strip.appendChild(tabNode(state.tabs[i]));
    }
  }

  function tabNode(tab) {
    var node = document.createElement('div');
    node.className = 'tab';
    if (tab.id === state.active) { node.className += ' active'; }
    if (tab.pinned) { node.className += ' pinned'; }
    if (tab.discarded) { node.className += ' discarded'; }
    if (tab.sleeping) { node.className += ' sleeping'; }
    node.setAttribute('role', 'tab');
    node.dataset.id = String(tab.id);
    node.title = (tab.discarded ? '(discarded) ' : tab.sleeping ? '(sleeping) ' : '') +
      tab.title + '\n' + tab.url;

    var icon = document.createElement('img');
    icon.className = 'tab-favicon';
    if (tab.favicon) {
      icon.src = tab.favicon;
      icon.dataset.empty = 'false';
    } else {
      icon.removeAttribute('src');
      icon.dataset.empty = 'true';
    }
    node.appendChild(icon);

    if (!tab.pinned) {
      var title = document.createElement('span');
      title.className = 'tab-title';
      title.textContent = tab.title || hostOf(tab.url) || 'New tab';
      node.appendChild(title);
    }

    if (tab.muted) {
      var muted = document.createElement('span');
      muted.className = 'tab-muted';
      muted.textContent = '🔇';
      node.appendChild(muted);
    } else if (tab.audible) {
      var audible = document.createElement('span');
      audible.className = 'tab-audible';
      audible.textContent = '🔊';
      node.appendChild(audible);
    }

    if (!tab.pinned) {
      var close = document.createElement('button');
      close.className = 'tab-close';
      close.textContent = '×';
      close.dataset.action = 'close';
      node.appendChild(close);
    }

    if (tab.loading) {
      var progress = document.createElement('div');
      progress.className = 'tab-progress';
      progress.style.width = '60%';
      node.appendChild(progress);
    }
    return node;
  }

  function renderToolbar() {
    var tab = activeTab();
    el.back.disabled = !(tab && tab.can_go_back);
    el.forward.disabled = !(tab && tab.can_go_forward);
    el.reload.textContent = tab && tab.loading ? '×' : '⟳';
    el.reload.title = tab && tab.loading ? 'Stop' : 'Reload (Ctrl+R)';

    if (document.activeElement !== el.omniboxInput) {
      el.omniboxInput.value = tab ? (tab.display_url || tab.url) : '';
    }
    if (tab) {
      el.omniboxIcon.textContent = tab.url && tab.url.indexOf('bir:') === 0 ? '◆' : '•';
      el.omniboxIcon.dataset.secure = tab.is_secure ? 'true' : 'false';
      el.omniboxIcon.dataset.insecure = tab.is_secure ? 'false' : 'true';
      el.omniboxIcon.title = tab.url && tab.url.indexOf('bir:') === 0
        ? 'Internal page'
        : (tab.is_secure ? 'Connection is secure' : 'Not secure') + ' — ' + hostOf(tab.url);
    }

    var starred = false;
    if (tab) {
      for (var i = 0; i < state.bookmarks.length; i++) {
        if (state.bookmarks[i].url === tab.url) { starred = true; break; }
      }
    }
    el.star.dataset.starred = starred ? 'true' : 'false';
    el.star.textContent = starred ? '★' : '☆';
  }

  function renderExtensions() {
    el.extButtons.textContent = '';
    for (var i = 0; i < state.extensions.length; i++) {
      var ext = state.extensions[i];
      if (!ext.enabled || !ext.has_popup) { continue; }
      var button = document.createElement('button');
      button.className = 'ext-button';
      button.title = ext.name;
      button.dataset.ext = ext.id;
      button.innerHTML = '<span>' + escapeHtml(ext.name.charAt(0).toUpperCase()) + '</span>';
      el.extButtons.appendChild(button);
    }
  }

  function renderSuggestions() {
    var box = el.suggestions;
    box.textContent = '';
    if (!state.suggestions.length || !el.omniboxInput.value) {
      box.hidden = true;
      return;
    }
    for (var i = 0; i < state.suggestions.length; i++) {
      var item = state.suggestions[i];
      var node = document.createElement('div');
      node.className = 'suggestion' + (i === state.selected ? ' selected' : '');
      node.dataset.url = item.url;
      node.dataset.index = String(i);

      var icon = document.createElement('span');
      icon.className = 'suggestion-icon';
      icon.textContent = iconFor(item.kind);
      node.appendChild(icon);

      var body = document.createElement('div');
      body.className = 'suggestion-body';
      var title = document.createElement('div');
      title.className = 'suggestion-title';
      title.textContent = item.title;
      var subtitle = document.createElement('div');
      subtitle.className = 'suggestion-subtitle';
      subtitle.textContent = item.subtitle || item.url;
      body.appendChild(title);
      body.appendChild(subtitle);
      node.appendChild(body);
      box.appendChild(node);
    }
    box.hidden = false;
  }

  function iconFor(kind) {
    switch (kind) {
      case 'url': return '→';
      case 'search': return '⌕';
      case 'history': return '⟲';
      case 'bookmark': return '★';
      case 'tab': return '▤';
      case 'internal': return '◆';
      default: return '→';
    }
  }

  function renderMenu() {
    var panel = el.menuPanel;
    panel.textContent = '';
    var tab = activeTab();
    var items = [
      { label: 'New tab', shortcut: 'Ctrl+T', run: function () { newTab(); } },
      { label: 'New window', shortcut: 'Ctrl+N', run: function () { send({ t: 'new_window', private: false }); } },
      { label: 'New private window', shortcut: 'Ctrl+Shift+N', run: function () { send({ t: 'new_window', private: true }); } },
      { separator: true },
      { label: 'History', shortcut: 'Ctrl+H', run: function () { send({ t: 'open_panel', panel: 'history' }); } },
      { label: 'Bookmarks', shortcut: 'Ctrl+Shift+O', run: function () { send({ t: 'open_panel', panel: 'bookmarks' }); } },
      { label: 'Downloads', shortcut: 'Ctrl+J', run: function () { send({ t: 'open_panel', panel: 'downloads' }); } },
      { label: 'Extensions', shortcut: 'Ctrl+Shift+A', run: function () { send({ t: 'open_panel', panel: 'extensions' }); } },
      { separator: true },
      {
        label: (state.settings && state.settings.privacy.block_ads) ? 'Blocking is on' : 'Blocking is off',
        run: function () { toggleBlocking(); }
      },
      {
        label: (state.settings && state.settings.performance.gpu === 'software') ? 'GPU: software' : 'GPU: hardware',
        run: function () {
          var value = state.settings.performance.gpu === 'software' ? 'hardware' : 'software';
          send({ t: 'set_setting', path: 'performance.gpu', value: value });
        }
      },
      { separator: true },
      { label: 'Zoom in', shortcut: 'Ctrl++', run: function () { if (tab) { send({ t: 'zoom_in', tab: tab.id }); } } },
      { label: 'Zoom out', shortcut: 'Ctrl+-', run: function () { if (tab) { send({ t: 'zoom_out', tab: tab.id }); } } },
      { label: 'Reset zoom', shortcut: 'Ctrl+0', run: function () { if (tab) { send({ t: 'zoom_reset', tab: tab.id }); } } },
      { separator: true },
      { label: 'Find in page', shortcut: 'Ctrl+F', run: function () { openFind(); } },
      { label: 'Print', shortcut: 'Ctrl+P', run: function () { if (tab) { send({ t: 'print', tab: tab.id }); } } },
      { label: 'Developer tools', shortcut: 'F12', run: function () { if (tab) { send({ t: 'open_devtools', tab: tab.id }); } } },
      { separator: true },
      { label: 'Settings', shortcut: 'Ctrl+,', run: function () { send({ t: 'open_panel', panel: 'settings' }); } },
      { label: 'About BIR', run: function () { send({ t: 'open_panel', panel: 'about' }); } },
      { separator: true },
      { label: 'Quit', shortshort: true, run: function () { send({ t: 'quit' }); } }
    ];

    for (var i = 0; i < items.length; i++) {
      if (items[i].separator) {
        var rule = document.createElement('div');
        rule.className = 'menu-separator';
        panel.appendChild(rule);
        continue;
      }
      panel.appendChild(menuItemNode(items[i]));
    }
    panel.hidden = !panel.hidden ? false : true;
  }

  function menuItemNode(item) {
    var node = document.createElement('button');
    node.className = 'menu-item';
    var label = document.createElement('span');
    label.textContent = item.label;
    node.appendChild(label);
    if (item.shortcut) {
      var shortcut = document.createElement('span');
      shortcut.className = 'shortcut';
      shortcut.textContent = item.shortcut;
      node.appendChild(shortcut);
    }
    node.addEventListener('mousedown', function (event) {
      event.preventDefault();
      item.run();
      el.menuPanel.hidden = true;
    });
    return node;
  }

  function renderPrompt() {
    var box = el.permission;
    if (!state.prompt) { box.hidden = true; return; }
    box.textContent = '';
    var text = document.createElement('div');
    text.className = 'permission-text';
    text.innerHTML = '<b>' + escapeHtml(hostOf('https://' + state.prompt.origin)) +
      '</b> wants to use <b>' + escapeHtml(state.prompt.permission) + '</b>.';
    var allow = document.createElement('button');
    allow.className = 'pill primary';
    allow.textContent = 'Allow';
    var deny = document.createElement('button');
    deny.className = 'pill';
    deny.textContent = 'Deny';
    allow.addEventListener('click', function () {
      send({ t: 'answer_permission', token: state.prompt.token, allow: true, remember: false });
      state.prompt = null;
      renderPrompt();
    });
    deny.addEventListener('click', function () {
      send({ t: 'answer_permission', token: state.prompt.token, allow: false, remember: false });
      state.prompt = null;
      renderPrompt();
    });
    box.appendChild(text);
    box.appendChild(deny);
    box.appendChild(allow);
    box.hidden = false;
  }

  // ------------------------------------------------------------------ actions

  function newTab() {
    send({ t: 'new_tab', url: null, foreground: true, after: null });
  }

  function navigate(input) {
    var tab = activeTab();
    if (!tab) { return; }
    send({ t: 'navigate', tab: tab.id, url: input });
    el.omniboxInput.blur();
    hideSuggestions();
  }

  function hideSuggestions() {
    state.suggestions = [];
    state.selected = -1;
    el.suggestions.hidden = true;
  }

  function requestSuggestions() {
    var tab = activeTab();
    var text = el.omniboxInput.value;
    if (!tab || !text) { hideSuggestions(); return; }
    state.requestId += 1;
    send({ t: 'omnibox_input', tab: tab.id, text: text, request_id: state.requestId });
  }

  function toggleBlocking() {
    var on = !(state.settings.privacy.block_ads || state.settings.privacy.block_trackers);
    send({ t: 'set_setting', path: 'privacy.block_ads', value: on });
    send({ t: 'set_setting', path: 'privacy.block_trackers', value: on });
  }

  function openFind() {
    state.findOpen = true;
    el.findbar.hidden = false;
    el.findInput.focus();
    el.findInput.select();
  }

  function closeFind() {
    state.findOpen = false;
    el.findbar.hidden = true;
    var tab = activeTab();
    if (tab) { send({ t: 'stop_find', tab: tab.id }); }
    el.findCount.textContent = '';
  }

  function find(forward) {
    var tab = activeTab();
    if (!tab) { return; }
    state.findText = el.findInput.value;
    send({ t: 'find', tab: tab.id, text: state.findText, forward: forward !== false });
  }

  function toast(text, kind) {
    var node = document.createElement('div');
    node.className = 'toast';
    node.dataset.kind = kind || 'info';
    node.textContent = text;
    el.toasts.appendChild(node);
    setTimeout(function () {
      node.style.transition = 'opacity 200ms';
      node.style.opacity = '0';
      setTimeout(function () {
        if (node.parentNode) { node.parentNode.removeChild(node); }
      }, 220);
    }, 3600);
  }

  function showPopup(ext) {
    if (el.popup.dataset.ext === ext.id && !el.popup.hidden) {
      el.popup.hidden = true;
      return;
    }
    el.popup.textContent = '';
    var frame = document.createElement('iframe');
    frame.src = 'bir://' + ext.id + '/' + ext.popup_path;
    el.popup.appendChild(frame);
    el.popup.dataset.ext = ext.id;
    el.popup.hidden = false;
  }

  // ------------------------------------------------------------------- events

  function onEvent(event) {
    switch (event.t) {
      case 'bootstrap':
        state.theme = event.theme;
        state.settings = event.settings || {};
        state.tabs = event.tabs || [];
        state.active = event.active;
        state.extensions = event.extensions || [];
        document.documentElement.dataset.theme = event.theme;
        document.documentElement.dataset.compact = String(
          !!(state.settings.appearance && state.settings.appearance.compact));
        document.documentElement.dataset.tabs = state.settings.appearance &&
          state.settings.appearance.tab_layout === 'vertical' ? 'vertical' : 'horizontal';
        render();
        // Panels need their data before they can paint anything.
        send({ t: 'history_search', query: '', limit: 500 });
        break;

      case 'tabs':
        state.tabs = event.tabs || [];
        state.active = event.active;
        renderTabs();
        renderToolbar();
        break;

      case 'progress':
        for (var i = 0; i < state.tabs.length; i++) {
          if (state.tabs[i].id === event.tab) { state.tabs[i].loading = event.loading; }
        }
        renderTabs();
        renderToolbar();
        break;

      case 'title':
        setTabField(event.tab, 'title', event.title);
        break;

      case 'url':
        setTabField(event.tab, 'url', event.url);
        setTabField(event.tab, 'display_url', event.url);
        break;

      case 'favicon':
        setTabField(event.tab, 'favicon', event.data_url);
        break;

      case 'zoom':
        setTabField(event.tab, 'zoom', event.scale);
        break;

      case 'find_result':
        el.findCount.textContent = event.matches
          ? (event.current + ' of ' + event.matches)
          : (event.tab && el.findInput.value ? 'No results' : '');
        break;

      case 'suggestions':
        // A slow reply for an old query must not overwrite a newer one.
        if (event.request_id !== state.requestId) { break; }
        state.suggestions = event.items || [];
        state.selected = state.suggestions.length ? 0 : -1;
        renderSuggestions();
        break;

      case 'settings':
        state.settings = event.settings || {};
        render();
        break;

      case 'theme':
        state.theme = event.theme;
        document.documentElement.dataset.theme = event.theme;
        break;

      case 'history':
        state.history = event.entries || [];
        break;

      case 'bookmarks':
        state.bookmarks = flattenBookmarks(event.nodes || []);
        renderToolbar();
        break;

      case 'downloads':
        state.downloads = event.items || [];
        break;

      case 'extensions':
        state.extensions = event.items || [];
        renderExtensions();
        break;

      case 'permission_request':
        state.prompt = { token: event.token, origin: event.origin, permission: event.permission };
        renderPrompt();
        break;

      case 'stats':
        state.stats = event;
        break;

      case 'toast':
        toast(event.text, event.kind);
        break;

      case 'open_panel':
        // Rust asks the chrome to show an internal page; it is a tab navigation here.
        send({ t: 'open_panel', panel: event.panel });
        break;

      default:
        break;
    }
  }

  function setTabField(id, field, value) {
    for (var i = 0; i < state.tabs.length; i++) {
      if (state.tabs[i].id === id) {
        state.tabs[i][field] = value;
        if (field === 'url') { state.tabs[i].display_url = displayUrl(value); }
        break;
      }
    }
    renderTabs();
    renderToolbar();
  }

  function displayUrl(url) {
    if (!url) { return ''; }
    if (url.indexOf('bir://') === 0) { return url; }
    return url.replace(/^https?:\/\//i, '').replace(/^www\./i, '').replace(/\/$/, '');
  }

  // BookmarkNode is externally tagged: {"Folder":{...}} or {"Item":{...}}.
  function flattenBookmarks(nodes, out) {
    out = out || [];
    for (var i = 0; i < nodes.length; i++) {
      var node = nodes[i];
      if (node && node.Item) { out.push(node.Item); }
      else if (node && node.Folder) { flattenBookmarks(node.Folder.children || [], out); }
      else if (node && node.url) { out.push(node); }
    }
    return out;
  }

  // ------------------------------------------------------------------- wiring

  function wire() {
    el.tabs = $('tabs');
    el.newTab = $('new-tab');
    el.back = $('back');
    el.forward = $('forward');
    el.reload = $('reload');
    el.home = $('home');
    el.omnibox = $('omnibox');
    el.omniboxInput = $('omnibox-input');
    el.omniboxIcon = $('omnibox-icon');
    el.star = $('omnibox-star');
    el.suggestions = $('suggestions');
    el.extButtons = $('ext-buttons');
    el.downloads = $('downloads');
    el.menu = $('menu');
    el.menuPanel = $('menu-panel');
    el.progress = $('progress');
    el.findbar = $('findbar');
    el.findInput = $('find-input');
    el.findCount = $('find-count');
    el.findPrev = $('find-prev');
    el.findNext = $('find-next');
    el.findClose = $('find-close');
    el.popup = $('popup');
    el.permission = $('permission');
    el.toasts = $('toasts');

    el.newTab.addEventListener('click', newTab);

    el.back.addEventListener('click', function () {
      var tab = activeTab();
      if (tab) { send({ t: 'back', tab: tab.id }); }
    });
    el.forward.addEventListener('click', function () {
      var tab = activeTab();
      if (tab) { send({ t: 'forward', tab: tab.id }); }
    });
    el.reload.addEventListener('click', function () {
      var tab = activeTab();
      if (!tab) { return; }
      send(tab.loading ? { t: 'stop', tab: tab.id } : { t: 'reload', tab: tab.id });
    });
    el.home.addEventListener('click', function () {
      var tab = activeTab();
      if (tab) {
        send({ t: 'navigate', tab: tab.id, url: (state.settings.general || {}).home_url || 'bir://newtab' });
      }
    });

    el.downloads.addEventListener('click', function () { send({ t: 'open_panel', panel: 'downloads' }); });

    el.star.addEventListener('click', function () {
      var tab = activeTab();
      if (!tab) { return; }
      if (el.star.dataset.starred === 'true') {
        for (var i = 0; i < state.bookmarks.length; i++) {
          if (state.bookmarks[i].url === tab.url) {
            send({ t: 'remove_bookmark', id: state.bookmarks[i].id });
            break;
          }
        }
      } else {
        send({ t: 'add_bookmark', url: tab.url, title: tab.title, parent: null });
      }
      send({ t: 'history_search', query: '', limit: 0 });
    });

    // --- omnibox -------------------------------------------------------
    el.omniboxInput.addEventListener('input', requestSuggestions);
    el.omniboxInput.addEventListener('focus', function () {
      el.omniboxInput.select();
      requestSuggestions();
    });
    el.omniboxInput.addEventListener('blur', function () {
      // Delay so a click on a suggestion still lands.
      setTimeout(function () { hideSuggestions(); }, 120);
    });
    el.omniboxInput.addEventListener('keydown', function (event) {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        if (!state.suggestions.length) { return; }
        event.preventDefault();
        var delta = event.key === 'ArrowDown' ? 1 : -1;
        state.selected = (state.selected + delta + state.suggestions.length) % state.suggestions.length;
        renderSuggestions();
        return;
      }
      if (event.key === 'Escape') {
        hideSuggestions();
        var tab = activeTab();
        if (tab) { el.omniboxInput.value = tab.display_url || tab.url; }
        el.omniboxInput.blur();
        return;
      }
      if (event.key === 'Enter') {
        var chosen = state.suggestions[state.selected];
        navigate(chosen ? chosen.url : el.omniboxInput.value);
      }
    });

    el.suggestions.addEventListener('mousedown', function (event) {
      var node = event.target.closest ? event.target.closest('.suggestion') : null;
      if (!node) { return; }
      event.preventDefault();
      navigate(node.dataset.url);
    });

    // --- tabs ----------------------------------------------------------
    el.tabs.addEventListener('mousedown', function (event) {
      var node = event.target.closest ? event.target.closest('.tab') : null;
      if (!node) { return; }
      var id = Number(node.dataset.id);
      if (event.target.dataset.action === 'close') {
        event.preventDefault();
        event.stopPropagation();
        send({ t: 'close_tab', tab: id });
        return;
      }
      if (event.button === 1) {
        // Middle click closes, as in every other browser.
        event.preventDefault();
        send({ t: 'close_tab', tab: id });
        return;
      }
      if (event.button === 2) { return; }
      send({ t: 'activate_tab', tab: id });
    });
    el.tabs.addEventListener('dblclick', function (event) {
      if (!event.target.closest || !event.target.closest('.tabs')) { return; }
      if (event.target.closest('.tab')) { return; }
      newTab();
    });
    el.tabs.addEventListener('contextmenu', function (event) {
      var node = event.target.closest ? event.target.closest('.tab') : null;
      if (!node) { return; }
      event.preventDefault();
      tabContextMenu(Number(node.dataset.id), event.clientX, event.clientY);
    });

    // --- find ----------------------------------------------------------
    el.findInput.addEventListener('input', function () { find(true); });
    el.findInput.addEventListener('keydown', function (event) {
      if (event.key === 'Enter') {
        find(!event.shiftKey);
      } else if (event.key === 'Escape') {
        closeFind();
      }
    });
    el.findNext.addEventListener('click', function () { find(true); });
    el.findPrev.addEventListener('click', function () { find(false); });
    el.findClose.addEventListener('click', closeFind);

    // --- menu and popups ------------------------------------------------
    el.menu.addEventListener('mousedown', function (event) {
      event.preventDefault();
      if (!el.menuPanel.hidden) { el.menuPanel.hidden = true; return; }
      renderMenu();
      el.menuPanel.hidden = false;
      el.popup.hidden = true;
    });
    el.extButtons.addEventListener('mousedown', function (event) {
      var button = event.target.closest ? event.target.closest('.ext-button') : null;
      if (!button) { return; }
      event.preventDefault();
      for (var i = 0; i < state.extensions.length; i++) {
        if (state.extensions[i].id === button.dataset.ext) {
          showPopup(state.extensions[i]);
          break;
        }
      }
    });

    document.addEventListener('mousedown', function (event) {
      if (!el.menuPanel.hidden && !el.menuPanel.contains(event.target) && event.target !== el.menu) {
        el.menuPanel.hidden = true;
      }
      if (!el.popup.hidden && !el.popup.contains(event.target) &&
          !(event.target.closest && event.target.closest('.ext-button'))) {
        el.popup.hidden = true;
      }
      if (!el.permission.hidden && !el.permission.contains(event.target)) {
        el.permission.hidden = true;
      }
    });

    // --- global keys ----------------------------------------------------
    // The chrome owns keys typed while it has focus; Rust owns the rest (see
    // `shortcuts.rs`), so the same shortcut works wherever the caret is.
    document.addEventListener('keydown', function (event) {
      var mod = navigator.platform.indexOf('Mac') === 0 ? event.metaKey : event.ctrlKey;
      if (!mod) { return; }
      var key = (event.key || '').toLowerCase();
      if (key === 'f') { event.preventDefault(); openFind(); }
      else if (key === 'l') { event.preventDefault(); el.omniboxInput.focus(); el.omniboxInput.select(); }
      else if (key === 't') { event.preventDefault(); newTab(); }
      else if (key === 'p' && event.shiftKey) { event.preventDefault(); send({ t: 'open_panel', panel: 'downloads' }); }
      else if (key === 'escape') { closeFind(); }
    }, false);

    // Right-click on dead chrome space should not pop the webview menu.
    document.addEventListener('contextmenu', function (event) {
      if (event.target === document.body || event.target === document.documentElement) {
        event.preventDefault();
      }
    });
  }

  function tabContextMenu(id, x, y) {
    var existing = $('tab-menu');
    if (existing && existing.parentNode) { existing.parentNode.removeChild(existing); }
    var tab = null;
    for (var i = 0; i < state.tabs.length; i++) {
      if (state.tabs[i].id === id) { tab = state.tabs[i]; break; }
    }
    if (!tab) { return; }

    var panel = document.createElement('div');
    panel.id = 'tab-menu';
    panel.className = 'menu-panel';
    panel.style.left = x + 'px';
    panel.style.top = y + 'px';
    panel.style.right = 'auto';

    var items = [
      { label: 'Reload', run: function () { send({ t: 'reload', tab: id }); } },
      { label: 'Duplicate', run: function () { send({ t: 'duplicate_tab', tab: id }); } },
      { label: tab.pinned ? 'Unpin' : 'Pin', run: function () { send({ t: 'pin_tab', tab: id, pinned: !tab.pinned }); } },
      { label: tab.muted ? 'Unmute' : 'Mute', run: function () { send({ t: 'mute_tab', tab: id, muted: !tab.muted }); } },
      { separator: true },
      { label: 'Close', run: function () { send({ t: 'close_tab', tab: id }); } },
      { label: 'Close other tabs', run: function () { send({ t: 'close_other_tabs', tab: id }); } }
    ];
    if (!tab.discarded && tab.id !== state.active) {
      items.splice(5, 0, {
        label: 'Free memory (discard)',
        run: function () { send({ t: 'discard_tab', tab: id }); }
      });
    }

    for (var j = 0; j < items.length; j++) {
      if (items[j].separator) {
        var rule = document.createElement('div');
        rule.className = 'menu-separator';
        panel.appendChild(rule);
        continue;
      }
      panel.appendChild(menuItemNode(items[j]));
    }
    document.body.appendChild(panel);
    panel.style.display = 'block';
    setTimeout(function () {
      document.addEventListener('mousedown', function handler(event) {
        if (panel.contains(event.target)) { return; }
        if (panel.parentNode) { panel.parentNode.removeChild(panel); }
        document.removeEventListener('mousedown', handler);
      });
    }, 0);
  }

  // -------------------------------------------------------------------- start

  window.bir = window.bir || {};
  window.bir.onEvent = onEvent;
  window.bir.send = send;

  // Hooks used by Rust-driven shortcuts (see `crate::shortcuts`).
  window.bir.focusOmnibox = function () {
    el.omniboxInput.focus();
    el.omniboxInput.select();
  };
  window.bir.focusFind = function () {
    openFind();
  };
  window.bir.findNext = function () {
    openFind();
    el.findInput.value = state.findText || '';
    find(true);
  };
  window.bir.stopFind = function () {
    closeFind();
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }

  function start() {
    wire();
    render();
    send({ t: 'ready' });
    // Ask for the data the chrome keeps locally for its own widgets.
    send({ t: 'history_search', query: '', limit: 0 });
    window.setInterval(function () { IDLE += 1; }, 1000);
  }
})();
