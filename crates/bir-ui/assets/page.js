/*
 * Internal pages.
 *
 * The page name lives in `document.body.dataset.page`; `page.js` renders the matching
 * section and re-renders whenever new state arrives from Rust.
 *
 * Commands go out over the same channel the chrome uses. `window.bir.tabId` is injected
 * by Rust when the webview is created, so a page can talk about its own tab; sending 0
 * also works, because the shell falls back to the window's active tab.
 */
(function () {
  'use strict';

  var state = {
    page: 'newtab',
    settings: null,
    engines: [],
    history: [],
    bookmarks: [],
    downloads: [],
    extensions: [],
    stats: null,
    query: ''
  };

  var content = null;

  function send(command) { window.ipc.postMessage(JSON.stringify(command)); }
  function tabId() { return (window.bir && window.bir.tabId) || 0; }
  function escapeHtml(text) {
    return String(text === null || text === undefined ? '' : text)
      .replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }
  function hostOf(url) {
    try { return new URL(url).host; } catch (e) { return url || ''; }
  }
  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) { node.className = className; }
    if (text !== undefined) { node.textContent = text; }
    return node;
  }
  function node(html) {
    var wrapper = document.createElement('div');
    wrapper.innerHTML = html;
    return wrapper.firstElementChild;
  }
  function humanBytes(bytes) {
    var units = ['B', 'KB', 'MB', 'GB', 'TB'];
    var value = Number(bytes) || 0;
    var index = 0;
    while (value >= 1024 && index < units.length - 1) { value /= 1024; index += 1; }
    return (index === 0 ? value : value.toFixed(1)) + ' ' + units[index];
  }
  function when(seconds) {
    if (!seconds) { return ''; }
    var date = new Date(seconds * 1000);
    var today = new Date();
    var sameDay = date.toDateString() === today.toDateString();
    if (sameDay) {
      return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
    }
    var yesterday = new Date(today.getTime() - 86400000);
    if (date.toDateString() === yesterday.toDateString()) { return 'Yesterday'; }
    return date.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  }
  function dayLabel(seconds) {
    var date = new Date(seconds * 1000);
    var today = new Date();
    if (date.toDateString() === today.toDateString()) { return 'Today'; }
    var yesterday = new Date(today.getTime() - 86400000);
    if (date.toDateString() === yesterday.toDateString()) { return 'Yesterday'; }
    var diff = (today - date) / 86400000;
    if (diff < 7) { return date.toLocaleDateString(undefined, { weekday: 'long' }); }
    return date.toLocaleDateString(undefined, { month: 'long', day: 'numeric', year: 'numeric' });
  }

  function set(path, value) { send({ t: 'set_setting', path: path, value: value }); }

  // ------------------------------------------------------------------- render

  function render() {
    if (!content) { return; }
    content.textContent = '';
    var section = document.getElementById('page-section');
    var sectionNames = {
      newtab: 'HOME', history: 'HISTORY', bookmarks: 'BOOKMARKS',
      downloads: 'DOWNLOADS', settings: 'SETTINGS', extensions: 'EXTENSIONS',
      about: 'ABOUT BIR'
    };
    if (section) { section.textContent = sectionNames[state.page] || 'BROWSER'; }
    document.title = (sectionNames[state.page] || 'BROWSER') + ' — BIR';
    var buttons = document.querySelectorAll('.nav button');
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].className = buttons[i].dataset.nav === state.page ? 'active' : '';
    }
    switch (state.page) {
      case 'newtab': renderNewTab(); break;
      case 'history': renderHistory(); break;
      case 'bookmarks': renderBookmarks(); break;
      case 'downloads': renderDownloads(); break;
      case 'settings': renderSettings(); break;
      case 'extensions': renderExtensions(); break;
      case 'about': renderAbout(); break;
      default: renderNewTab(); break;
    }
  }

  function heading(title, lede) {
    content.appendChild(el('h1', null, title));
    if (lede) { content.appendChild(el('p', 'lede', lede)); }
  }

  // ------------------------------------------------------------------ new tab

  function renderNewTab() {
    var clock = el('div', 'clock');
    var now = new Date();
    clock.textContent = now.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });
    content.appendChild(clock);
    content.appendChild(el('div', 'clock-sub',
      now.toLocaleDateString(undefined, { weekday: 'long', month: 'long', day: 'numeric' })));

    var form = el('div', 'newtab-search');
    var input = document.createElement('input');
    input.type = 'text';
    input.placeholder = 'Search the web, or type a URL';
    input.spellcheck = false;
    var submit = el('button', 'action primary', 'Go');
    form.appendChild(input);
    form.appendChild(submit);
    content.appendChild(form);

    submit.addEventListener('click', function () {
      var value = input.value.trim();
      if (value) { send({ t: 'navigate', tab: tabId(), url: value }); }
    });
    input.addEventListener('keydown', function (event) {
      if (event.key === 'Enter' && input.value.trim()) {
        send({ t: 'navigate', tab: tabId(), url: input.value.trim() });
      }
    });
    setTimeout(function () { input.focus(); }, 30);

    // Top sites: the hosts the user actually returns to, from local history.
    var counts = {};
    for (var i = 0; i < state.history.length; i++) {
      var entry = state.history[i];
      var host = entry.domain || hostOf(entry.url);
      if (!host) { continue; }
      if (!counts[host]) { counts[host] = { host: host, url: entry.url, visits: 0, title: entry.title }; }
      counts[host].visits += Math.max(1, entry.visit_count || 1);
    }
    var top = Object.keys(counts).map(function (k) { return counts[k]; })
      .sort(function (a, b) { return b.visits - a.visits; }).slice(0, 8);

    content.appendChild(el('h2', null, 'Top sites'));
    if (!top.length) {
      content.appendChild(el('div', 'empty', 'Visit a few sites and they will show up here.'));
      return;
    }
    var grid = el('div', 'grid');
    for (var j = 0; j < top.length; j++) {
      grid.appendChild(topSiteTile(top[j]));
    }
    content.appendChild(grid);
  }

  function topSiteTile(site) {
    var tile = el('div', 'tile');
    tile.appendChild(el('div', 'letter', site.host.replace(/^www\./, '').charAt(0)));
    var body = el('div', 'grow');
    body.appendChild(el('div', 'tile-title', site.host.replace(/^www\./, '')));
    body.appendChild(el('div', 'small muted', site.visits + ' visits'));
    tile.appendChild(body);
    tile.addEventListener('click', function () {
      send({ t: 'navigate', tab: tabId(), url: site.url });
    });
    return tile;
  }

  // ------------------------------------------------------------------ history

  function renderHistory() {
    heading('History', 'Everything you visited, stored locally. Nothing leaves this machine.');

    var bar = el('div', 'row');
    var search = document.createElement('input');
    search.type = 'search';
    search.placeholder = 'Search history';
    search.className = 'wide';
    search.value = state.query;
    var clear = el('button', 'action danger', 'Clear all history');
    bar.appendChild(search);
    bar.appendChild(clear);
    content.appendChild(bar);

    search.addEventListener('input', function () {
      state.query = search.value;
      send({ t: 'history_search', query: state.query, limit: 500 });
    });
    clear.addEventListener('click', function () { send({ t: 'clear_history' }); });

    var list = el('div', 'list');
    var groups = {};
    var order = [];
    for (var i = 0; i < state.history.length; i++) {
      var entry = state.history[i];
      var label = dayLabel(entry.visit_at || 0);
      if (!groups[label]) { groups[label] = []; order.push(label); }
      groups[label].push(entry);
    }
    for (var g = 0; g < order.length; g++) {
      list.appendChild(el('h2', null, order[g]));
      var items = groups[order[g]];
      for (var k = 0; k < items.length; k++) {
        list.appendChild(historyRow(items[k]));
      }
    }
    if (!state.history.length) {
      list.appendChild(el('div', 'empty', 'No history yet.'));
    }
    content.appendChild(list);
    content.appendChild(el('p', 'small muted',
      'History is capped at 100,000 entries / 12 MiB; oldest entries are dropped first.'));
  }

  function historyRow(entry) {
    var row = el('div', 'list-item');
    var body = el('div', 'grow');
    var link = document.createElement('a');
    link.href = entry.url;
    link.textContent = entry.title || entry.url;
    link.className = 'title';
    link.addEventListener('click', function (event) {
      event.preventDefault();
      send({ t: 'navigate', tab: tabId(), url: entry.url });
    });
    var sub = el('div', 'sub');
    sub.textContent = entry.url + '  ·  ' + when(entry.visit_at || 0);
    body.appendChild(link);
    body.appendChild(sub);
    row.appendChild(body);
    return row;
  }

  // ---------------------------------------------------------------- bookmarks

  function renderBookmarks() {
    heading('Bookmarks', 'Stored as newline-delimited JSON in your profile, so any editor can read them.');
    var search = document.createElement('input');
    search.type = 'search';
    search.placeholder = 'Search bookmarks';
    search.className = 'wide';
    search.value = state.query;
    content.appendChild(search);
    search.addEventListener('input', function () {
      state.query = search.value.toLowerCase();
      render();
      var next = document.querySelector('input[type="search"]');
      if (next) { next.focus(); }
    });

    var list = el('div', 'list');
    var shown = 0;
    for (var i = 0; i < state.bookmarks.length; i++) {
      var bookmark = state.bookmarks[i];
      if (state.query &&
          (bookmark.title + ' ' + bookmark.url).toLowerCase().indexOf(state.query) === -1) {
        continue;
      }
      shown += 1;
      list.appendChild(bookmarkRow(bookmark));
    }
    if (!shown) { list.appendChild(el('div', 'empty', 'No bookmarks yet.')); }
    content.appendChild(list);
  }

  function bookmarkRow(bookmark) {
    var row = el('div', 'list-item');
    var body = el('div', 'grow');
    var link = document.createElement('a');
    link.href = bookmark.url;
    link.className = 'title';
    link.textContent = bookmark.title || bookmark.url;
    link.addEventListener('click', function (event) {
      event.preventDefault();
      send({ t: 'navigate', tab: tabId(), url: bookmark.url });
    });
    body.appendChild(link);
    body.appendChild(el('div', 'sub', bookmark.url));
    row.appendChild(body);
    var remove = el('button', 'icon-button', '🗑');
    remove.title = 'Remove bookmark';
    remove.addEventListener('click', function () {
      send({ t: 'remove_bookmark', id: bookmark.id });
    });
    row.appendChild(remove);
    return row;
  }

  // ---------------------------------------------------------------- downloads

  function renderDownloads() {
    heading('Downloads', state.settings && state.settings.downloads.dir
      ? 'Saved to ' + state.settings.downloads.dir : 'Downloads are saved to your downloads folder.');

    var list = el('div', 'list');
    for (var i = 0; i < state.downloads.length; i++) {
      list.appendChild(downloadRow(state.downloads[i]));
    }
    if (!state.downloads.length) {
      list.appendChild(el('div', 'empty', 'Nothing downloaded yet.'));
    }
    content.appendChild(list);

    var row = el('div', 'row');
    var open = el('button', 'action', 'Open downloads folder');
    var clearFinished = el('button', 'action', 'Clear finished');
    open.addEventListener('click', function () {
      // Empty id + reveal = "show me the folder everything lands in".
      send({ t: 'download_action', id: '', action: 'reveal' });
    });
    clearFinished.addEventListener('click', function () {
      send({ t: 'download_action', id: '', action: 'clear_finished' });
    });
    row.appendChild(open);
    row.appendChild(clearFinished);
    content.appendChild(row);
  }

  function downloadRow(item) {
    var row = el('div', 'list-item');
    var body = el('div', 'grow');
    body.appendChild(el('div', 'title', item.filename || item.url));
    var progress = (item.total && item.total > 0) ? (item.received / item.total) : null;
    var sub;
    if (item.state === 'in_progress' && progress !== null) {
      var bar = el('div', 'bar');
      var fill = document.createElement('i');
      fill.style.width = Math.round(progress * 100) + '%';
      bar.appendChild(fill);
      body.appendChild(bar);
      sub = el('div', 'sub', humanBytes(item.received) + ' of ' + humanBytes(item.total));
    } else if (item.state === 'in_progress') {
      sub = el('div', 'sub', humanBytes(item.received) + ' downloaded');
    } else {
      sub = el('div', 'sub', statusText(item));
    }
    body.appendChild(sub);
    row.appendChild(body);

    if (item.state === 'in_progress') {
      row.appendChild(downloadButton('×', 'Cancel', function () {
        send({ t: 'download_action', id: item.id, action: 'cancel' });
      }));
    } else if (item.state === 'complete') {
      row.appendChild(downloadButton('↗', 'Open', function () {
        send({ t: 'download_action', id: item.id, action: 'open' });
      }));
    } else if (item.state === 'failed' || item.state === 'cancelled') {
      row.appendChild(downloadButton('⟳', 'Retry', function () {
        send({ t: 'download_action', id: item.id, action: 'retry' });
      }));
    }
    row.appendChild(downloadButton('🗑', 'Remove from list', function () {
      send({ t: 'download_action', id: item.id, action: 'remove' });
    }));
    return row;
  }

  function statusText(item) {
    if (item.state === 'complete') { return humanBytes(item.total || item.received) + ' · finished'; }
    if (item.state === 'cancelled') { return 'cancelled'; }
    if (item.state === 'failed') { return 'failed'; }
    if (item.state === 'starting') { return 'starting'; }
    return 'in progress';
  }

  function downloadButton(glyph, title, run) {
    var button = el('button', 'icon-button', glyph);
    button.title = title;
    button.addEventListener('click', run);
    return button;
  }

  // ----------------------------------------------------------------- settings

  function renderSettings() {
    heading('Settings', 'Every change is written to disk immediately.');
    var s = state.settings || {};

    content.appendChild(section('Appearance', [
      selectRow('Theme', 'appearance.theme', s.appearance && s.appearance.theme,
        ['system', 'light', 'dark']),
      selectRow('Tab layout', 'appearance.tab_layout', s.appearance && s.appearance.tab_layout,
        ['horizontal', 'vertical']),
      toggleRow('Compact chrome', 'appearance.compact', !!(s.appearance && s.appearance.compact)),
      toggleRow('Show bookmarks bar', 'appearance.show_bookmarks_bar',
        !!(s.appearance && s.appearance.show_bookmarks_bar)),
      numberRow('Default zoom', 'appearance.default_zoom',
        s.appearance && s.appearance.default_zoom, 0.25, 5, 0.05),
      textRow('Home page', 'general.home_url', s.general && s.general.home_url),
      textRow('New tab page', 'general.new_tab_url', s.general && s.general.new_tab_url),
      selectRow('On startup', 'general.startup', s.general && s.general.startup,
        ['open_home', 'blank', 'restore_session'])
    ]));

    content.appendChild(section('Privacy & blocking', [
      toggleRow('Block ads', 'privacy.block_ads', !!(s.privacy && s.privacy.block_ads)),
      toggleRow('Block trackers', 'privacy.block_trackers', !!(s.privacy && s.privacy.block_trackers)),
      toggleRow('Block cosmetic rules (element hiding)', 'privacy.block_cosmetic',
        !!(s.privacy && s.privacy.block_cosmetic)),
      toggleRow('Strip tracking parameters from URLs', 'privacy.strip_tracking_params',
        !!(s.privacy && s.privacy.strip_tracking_params)),
      toggleRow('HTTPS-only mode', 'privacy.https_only', !!(s.privacy && s.privacy.https_only)),
      toggleRow('Send Do-Not-Track', 'privacy.do_not_track', !!(s.privacy && s.privacy.do_not_track)),
      toggleRow('Resist fingerprinting', 'privacy.resist_fingerprinting',
        !!(s.privacy && s.privacy.resist_fingerprinting)),
      selectRow('Clear on exit', 'privacy.clear_data_on_exit',
        s.privacy && s.privacy.clear_data_on_exit, ['nothing', 'history', 'cookies_and_storage', 'everything'])
    ]));

    content.appendChild(section('Performance', [
      selectRow('GPU mode', 'performance.gpu', s.performance && s.performance.gpu,
        ['hardware', 'software']),
      numberRow('Live webviews before sleeping starts', 'performance.max_live_webviews',
        s.performance && s.performance.max_live_webviews, 1, 64, 1),
      numberRow('Sleep a background tab after (seconds)', 'performance.sleep_after_secs',
        s.performance && s.performance.sleep_after_secs, 0, 86400, 30),
      numberRow('Discard a slept tab after (seconds)', 'performance.discard_after_secs',
        s.performance && s.performance.discard_after_secs, 0, 604800, 60),
      toggleRow('Discard tabs under memory pressure', 'performance.discard_under_pressure',
        !!(s.performance && s.performance.discard_under_pressure)),
      numberRow('Memory pressure threshold (%)', 'performance.memory_pressure_percent',
        s.performance && s.performance.memory_pressure_percent, 40, 99, 1),
      toggleRow('Throttle background tabs', 'performance.background_throttling',
        !!(s.performance && s.performance.background_throttling)),
      toggleRow('Lazy session restore', 'performance.lazy_session_restore',
        !!(s.performance && s.performance.lazy_session_restore))
    ]));

    content.appendChild(liveStats());

    content.appendChild(section('Downloads', [
      textRow('Download folder', 'downloads.dir', s.downloads && s.downloads.dir),
      toggleRow('Ask where to save each file', 'downloads.ask_where_to_save',
        !!(s.downloads && s.downloads.ask_where_to_save)),
      numberRow('Maximum concurrent downloads', 'downloads.max_concurrent',
        s.downloads && s.downloads.max_concurrent, 1, 16, 1)
    ]));

    content.appendChild(section('Network & pages', [
      toggleRow('Allow autoplay', 'network.allow_autoplay', !!(s.network && s.network.allow_autoplay)),
      toggleRow('Enable WebGL', 'network.enable_webgl', !!(s.network && s.network.enable_webgl)),
      textRow('User agent (blank = default)', 'network.user_agent',
        s.network && s.network.user_agent),
      toggleRow('JavaScript', 'advanced.javascript', !(s.advanced && s.advanced.javascript === false)),
      toggleRow('Developer tools enabled', 'advanced.devtools', !!(s.advanced && s.advanced.devtools)),
      toggleRow('Show the "blocked" page', 'advanced.show_blocked_page',
        !!(s.advanced && s.advanced.show_blocked_page))
    ]));

    content.appendChild(filterLists(s));
    content.appendChild(searchEngines(s));
    content.appendChild(dataControls());
  }

  function section(title, rows) {
    var card = el('div', 'card');
    card.appendChild(el('h2', null, title));
    for (var i = 0; i < rows.length; i++) {
      if (rows[i]) { card.appendChild(rows[i]); }
    }
    return card;
  }

  function rowWrap(label, control) {
    var row = el('div', 'row spread');
    var text = el('div', 'grow');
    text.appendChild(el('div', null, label));
    row.appendChild(text);
    row.appendChild(control);
    return row;
  }

  function toggleRow(label, path, value) {
    var labelNode = el('label', 'switch');
    var input = document.createElement('input');
    input.type = 'checkbox';
    input.checked = !!value;
    labelNode.appendChild(input);
    input.addEventListener('change', function () { set(path, input.checked); });
    return rowWrap(label, labelNode);
  }

  function selectRow(label, path, value, options) {
    var select = document.createElement('select');
    for (var i = 0; i < options.length; i++) {
      var option = document.createElement('option');
      option.value = options[i];
      option.textContent = options[i].replace(/_/g, ' ');
      if (value === options[i]) { option.selected = true; }
      select.appendChild(option);
    }
    select.addEventListener('change', function () { set(path, select.value); });
    return rowWrap(label, select);
  }

  function numberRow(label, path, value, min, max, step) {
    var input = document.createElement('input');
    input.type = 'number';
    input.min = String(min);
    input.max = String(max);
    input.step = String(step);
    input.value = String(value === undefined || value === null ? 0 : value);
    input.addEventListener('change', function () {
      var parsed = Number(input.value);
      if (!isNaN(parsed)) { set(path, parsed); }
    });
    return rowWrap(label, input);
  }

  function textRow(label, path, value) {
    var input = document.createElement('input');
    input.type = 'text';
    input.className = 'wide';
    input.value = value === undefined || value === null ? '' : String(value);
    input.addEventListener('change', function () { set(path, input.value); });
    var row = el('div', 'row');
    row.appendChild(el('div', 'grow', label));
    row.appendChild(input);
    return row;
  }

  function liveStats() {
    var card = el('div', 'card');
    card.appendChild(el('h2', null, 'Resource usage'));
    var stats = state.stats;
    if (!stats) {
      card.appendChild(el('div', 'small muted', 'Waiting for the first sample…'));
      return card;
    }
    card.appendChild(statRow('Browser process (RSS)', stats.rss_mib + ' MiB'));
    card.appendChild(statRow('Estimated webview total', stats.webview_mib + ' MiB'));
    card.appendChild(statRow('CPU', stats.cpu_percent.toFixed(1) + '% of one core'));
    card.appendChild(statRow('System memory in use', stats.system_used_percent + '%'));
    card.appendChild(statRow('Tabs live / sleeping / discarded',
      stats.tabs_live + ' / ' + stats.tabs_sleeping + ' / ' + stats.tabs_discarded));
    return card;
  }

  function statRow(label, value) {
    var row = el('div', 'row spread');
    row.appendChild(el('span', 'muted', label));
    row.appendChild(el('span', 'mono', value));
    return row;
  }

  function filterLists(s) {
    var card = el('div', 'card');
    card.appendChild(el('h2', null, 'Filter lists'));
    var lists = (s.advanced && s.advanced.filter_lists) || [];
    if (!lists.length) {
      card.appendChild(el('div', 'small muted', 'No lists configured.'));
      return card;
    }
    for (var i = 0; i < lists.length; i++) {
      (function (index) {
        var list = lists[index];
        var row = el('div', 'row spread');
        var body = el('div', 'grow');
        body.appendChild(el('div', null, list.name));
        body.appendChild(el('div', 'small muted', list.url + (list.builtin ? ' · built in' : '')));
        row.appendChild(body);
        row.appendChild(el('span', 'badge ' + (list.enabled ? 'ok' : ''),
          list.enabled ? 'enabled' : 'off'));
        card.appendChild(row);
      })(i);
    }
    card.appendChild(el('p', 'small muted',
      'Lists are cached for up to three days and refreshed in the background.'));
    return card;
  }

  function searchEngines(s) {
    var card = el('div', 'card');
    card.appendChild(el('h2', null, 'Search engines'));
    var current = (s.general && s.general.default_search) || 'duckduckgo';
    for (var i = 0; i < state.engines.length; i++) {
      (function (engine) {
        var row = el('div', 'row spread');
        var body = el('div', 'grow');
        body.appendChild(el('div', null, engine.title));
        body.appendChild(el('div', 'small muted',
          (engine.keyword ? engine.keyword + ' ' : '') + engine.search_url));
        row.appendChild(body);
        if (engine.name === current) {
          row.appendChild(el('span', 'badge ok', 'default'));
        } else {
          var make = el('button', 'action', 'Make default');
          make.addEventListener('click', function () { set('general.default_search', engine.name); });
          row.appendChild(make);
        }
        card.appendChild(row);
      })(state.engines[i]);
    }
    card.appendChild(el('p', 'small muted',
      'Type an engine keyword followed by a space in the address bar to search with it.'));
    return card;
  }

  function dataControls() {
    var card = el('div', 'card');
    card.appendChild(el('h2', null, 'Clear browsing data'));
    var row = el('div', 'row');
    var history = el('button', 'action', 'Clear history');
    var cookies = el('button', 'action', 'Clear cookies and site data');
    var cache = el('button', 'action danger', 'Clear everything');
    history.addEventListener('click', function () {
      send({ t: 'clear_browsing_data', history: true, cookies: false, cache: false });
    });
    cookies.addEventListener('click', function () {
      send({ t: 'clear_browsing_data', history: false, cookies: true, cache: false });
    });
    cache.addEventListener('click', function () {
      send({ t: 'clear_browsing_data', history: true, cookies: true, cache: true });
    });
    row.appendChild(history);
    row.appendChild(cookies);
    row.appendChild(cache);
    card.appendChild(row);
    return card;
  }

  // --------------------------------------------------------------- extensions

  function renderExtensions() {
    heading('Extensions', 'Chrome MV3 extensions, installed from a .crx, a .zip, or a folder on disk.');

    if (state.settings && state.settings.extensions && !state.settings.extensions.enabled) {
      var warn = el('div', 'card');
      warn.appendChild(el('div', null, 'Extensions are switched off.'));
      var enable = el('button', 'action primary', 'Turn on');
      enable.addEventListener('click', function () { set('extensions.enabled', true); });
      warn.appendChild(enable);
      content.appendChild(warn);
    }

    var tools = el('div', 'card');
    tools.appendChild(el('h2', null, 'Install'));

    var fileRow = el('div', 'row');
    var file = document.createElement('input');
    file.type = 'file';
    file.accept = '.crx,.zip';
    fileRow.appendChild(el('div', 'grow', 'Install a packaged extension (.crx or .zip)'));
    fileRow.appendChild(file);
    file.addEventListener('change', function () {
      if (!file.files || !file.files[0]) { return; }
      installFile(file.files[0]);
    });
    tools.appendChild(fileRow);

    var dirRow = el('div', 'row');
    var dirInput = document.createElement('input');
    dirInput.type = 'text';
    dirInput.className = 'wide';
    dirInput.placeholder = '/path/to/unpacked/extension';
    var dirButton = el('button', 'action', 'Load unpacked');
    dirRow.appendChild(dirInput);
    dirRow.appendChild(dirButton);
    dirButton.addEventListener('click', function () {
      if (dirInput.value.trim()) {
        send({ t: 'install_extension', path: dirInput.value.trim() });
      }
    });
    tools.appendChild(dirRow);

    var nativeRow = el('div', 'row');
    nativeRow.appendChild(el('div', 'grow', 'Let WebView2 load extensions natively (Windows only)'));
    nativeRow.appendChild(settingToggle('extensions.native_webview2_extensions',
      !!(state.settings && state.settings.extensions &&
         state.settings.extensions.native_webview2_extensions)));
    tools.appendChild(nativeRow);
    tools.appendChild(el('p', 'small muted',
      'On Windows, WebView2 can host extensions itself; elsewhere BIR runs them through its own ' +
      'host (background page, content scripts, storage, tabs, alarms, menus and notifications).'));
    content.appendChild(tools);

    var list = el('div', 'list');
    for (var i = 0; i < state.extensions.length; i++) {
      list.appendChild(extensionCard(state.extensions[i]));
    }
    if (!state.extensions.length) {
      list.appendChild(el('div', 'empty', 'No extensions installed.'));
    }
    content.appendChild(list);
  }

  function settingToggle(path, value) {
    var label = el('label', 'switch');
    var input = document.createElement('input');
    input.type = 'checkbox';
    input.checked = !!value;
    input.addEventListener('change', function () { set(path, input.checked); });
    label.appendChild(input);
    return label;
  }

  function installFile(file) {
    var reader = new FileReader();
    reader.onload = function () {
      var text = String(reader.result || '');
      // Strip the data-URL prefix; Rust wants raw base64.
      var comma = text.indexOf(',');
      var data = comma >= 0 ? text.slice(comma + 1) : text;
      send({ t: 'install_extension_data', name: file.name, data: data });
    };
    reader.readAsDataURL(file);
  }

  function extensionCard(ext) {
    var card = el('div', 'card');
    var row = el('div', 'ext-card');
    row.appendChild(el('div', 'ext-icon', ext.name.charAt(0)));
    var body = el('div', 'grow');
    var titleRow = el('div', 'row');
    titleRow.appendChild(el('div', null, ext.name + '  '));
    titleRow.appendChild(el('span', 'badge', 'v' + ext.version));
    titleRow.appendChild(el('span', 'badge', ext.kind));
    if (!ext.enabled) { titleRow.appendChild(el('span', 'badge warn', 'off')); }
    body.appendChild(titleRow);
    if (ext.description) { body.appendChild(el('div', 'small muted', ext.description)); }
    body.appendChild(el('div', 'small muted mono', ext.id));
    if (ext.error) {
      body.appendChild(el('div', 'small', '⚠ ' + ext.error));
    }
    var perms = el('div', 'ext-perms');
    for (var i = 0; i < ext.permissions.length; i++) {
      perms.appendChild(el('span', 'badge', ext.permissions[i]));
    }
    body.appendChild(perms);
    row.appendChild(body);

    var actions = el('div', 'row');
    actions.appendChild(callbackToggle(ext.enabled, function (on) {
      send({ t: 'set_extension_enabled', id: ext.id, enabled: on });
    }));
    actions.appendChild(button('Reload', function () {
      send({ t: 'reload_extension', id: ext.id });
    }));
    if (ext.has_options) {
      actions.appendChild(button('Options', function () {
        send({ t: 'open_extension_options', id: ext.id });
      }));
    }
    actions.appendChild(button('Remove', function () {
      send({ t: 'remove_extension', id: ext.id });
    }, 'danger'));
    row.appendChild(actions);
    card.appendChild(row);
    return card;
  }

  function callbackToggle(initial, onChange) {
    var label = el('label', 'switch');
    var input = document.createElement('input');
    input.type = 'checkbox';
    input.checked = !!initial;
    input.title = 'Enabled';
    input.addEventListener('change', function () { onChange(input.checked); });
    label.appendChild(input);
    return label;
  }

  function button(label, run, kind) {
    var node = el('button', 'action' + (kind ? ' ' + kind : ''), label);
    node.addEventListener('click', run);
    return node;
  }

  // -------------------------------------------------------------------- about

  function renderAbout() {
    heading('BIR', 'A browser built on each platform’s own webview.');
    var card = el('div', 'card');
    var rows = [
      ['Engine', 'System webview — WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux'],
      ['Shell', 'wry + tao, rendered composite: HTML chrome above one webview per tab'],
      ['Extensions', 'Chrome MV3 compatibility layer (unpacked, .zip, .crx3)'],
      ['Storage', 'Newline-delimited JSON, written atomically — no database to corrupt'],
      ['Version', '0.1.0']
    ];
    for (var i = 0; i < rows.length; i++) {
      var row = el('div', 'row spread');
      row.appendChild(el('span', 'muted', rows[i][0]));
      row.appendChild(el('span', null, rows[i][1]));
      card.appendChild(row);
    }
    content.appendChild(card);

    var notes = el('div', 'card');
    notes.appendChild(el('h2', null, 'What is honest about this build'));
    notes.appendChild(el('ul', null, ''));
    var list = document.createElement('ul');
    var items = [
      'Content scripts run in the page’s own JavaScript world: a system webview gives us ' +
        'no isolated world, so a page can see them.',
      'webRequest and declarativeNetRequest reject: intercepting network traffic is not ' +
        'possible through a system webview. The built-in blocker replaces them.',
      'Cookie APIs are unavailable for the same reason; per-site storage is cleared ' +
        'through the webview’s own data APIs.',
      'On Windows, extensions can additionally be handed to WebView2’s native loader, ' +
        'which is the closest thing to real Chrome extension hosting available here.'
    ];
    for (var j = 0; j < items.length; j++) {
      list.appendChild(el('li', 'small muted', items[j]));
    }
    notes.appendChild(list);
    content.appendChild(notes);
  }

  // ------------------------------------------------------------------- events

  function onEvent(event) {
    switch (event.t) {
      case 'settings':
        state.settings = event.settings || {};
        document.documentElement.dataset.theme = (state.settings.appearance || {}).theme || 'system';
        render();
        break;
      case 'history':
        state.history = event.entries || [];
        render();
        break;
      case 'bookmarks':
        state.bookmarks = flatten(event.nodes || []);
        render();
        break;
      case 'downloads':
        state.downloads = event.items || [];
        render();
        break;
      case 'extensions':
        state.extensions = event.items || [];
        render();
        break;
      case 'search_engines':
        state.engines = event.engines || [];
        render();
        break;
      case 'stats':
        state.stats = event;
        if (state.page === 'settings') { render(); }
        break;
      case 'theme':
        document.documentElement.dataset.theme = event.theme;
        break;
      case 'toast':
        flash(event.text);
        break;
      default:
        break;
    }
  }

  // BookmarkNode is externally tagged: {"Folder":{...}} or {"Item":{...}}.
  function flatten(nodes, out) {
    out = out || [];
    for (var i = 0; i < nodes.length; i++) {
      var node = nodes[i];
      if (node && node.Item) { out.push(node.Item); }
      else if (node && node.Folder) { flatten(node.Folder.children || [], out); }
      else if (node && node.url) { out.push(node); }
    }
    return out;
  }

  function flash(text) {
    var node = el('div', 'card', text);
    node.style.position = 'fixed';
    node.style.right = '20px';
    node.style.bottom = '20px';
    node.style.zIndex = '40';
    document.body.appendChild(node);
    setTimeout(function () {
      if (node.parentNode) { node.parentNode.removeChild(node); }
    }, 3000);
  }

  // -------------------------------------------------------------------- start

  window.bir = window.bir || {};
  window.bir.onEvent = onEvent;
  window.bir.send = send;

  function start() {
    content = document.getElementById('content');
    state.page = document.body.dataset.page || 'newtab';

    var nav = document.querySelector('.nav');
    nav.addEventListener('click', function (event) {
      var target = event.target.closest ? event.target.closest('button') : null;
      if (!target || !target.dataset.nav) { return; }
      send({ t: 'navigate', tab: tabId(), url: 'bir://' + target.dataset.nav });
    });

    render();
    send({ t: 'ready' });
    // Ask for the data this page needs; the shell pushes it back as events.
    send({ t: 'history_search', query: '', limit: 500 });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', start);
  } else {
    start();
  }
})();
