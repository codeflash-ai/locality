function renderWireframeDeck(){
  const navGroups = window.WIREFRAME_NAV_GROUPS || [];
  const screens = window.WIREFRAME_SCREENS || [];
  const navHtml = navGroups.map(group => `
    <div class="ngroup">${escapeHtml(group.label)}</div>
${group.items.map(item => `
    <a href="${escapeHtml(item.page)}" data-s="${escapeHtml(item.id)}"><span class="k">${escapeHtml(item.key)}</span>${escapeHtml(item.title)}</a>`).join('')}
  `).join('');
  const screenHtml = screens.map(screen => screen.html).join('\n\n');

  document.body.innerHTML = `
<div class="deck">
  <nav class="decknav" aria-label="Screens">
    <div class="brand">
      <h1>Locality Desktop</h1>
      <div class="sub">WIREFRAME DECK · v7 LIVE SYNC FLOWS</div>
    </div>
${navHtml}
  </nav>

  <main class="stage">
    <div class="decktools" aria-label="Wireframe collaboration controls">
      <button class="btn btn-sm" type="button" id="toggle-design-notes">Hide design notes</button>
      <button class="btn btn-sm" type="button" id="copy-screen-link">Copy screen link</button>
      <span class="hint">Click app sidebar items, breadcrumbs, rows, and action buttons to test navigation paths. Screen links use separate HTML files.</span>
      <span class="status" id="deck-status" aria-live="polite"></span>
    </div>

${screenHtml}
  </main>
</div>`;
}

function escapeHtml(value){
  return String(value).replace(/[&<>"']/g, char => ({
    '&': '&amp;',
    '<': '&lt;',
    '>': '&gt;',
    '"': '&quot;',
    "'": '&#39;',
  })[char]);
}

renderWireframeDeck();

const nav = document.querySelectorAll('.decknav [data-s]');
  const screens = document.querySelectorAll('.screen');
  const deckStatus = document.getElementById('deck-status');
  const screenPages = Object.freeze(
    Object.fromEntries(
      (window.WIREFRAME_NAV_GROUPS || []).flatMap(group =>
        group.items.map(item => [item.id, item.page])
      )
    )
  );
  const pageScreens = Object.fromEntries(Object.entries(screenPages).map(([id, page]) => [page, id]));

  function screenHref(id){
    return screenPages[id] || screenPages.ob1;
  }

  function screenBasePath(pathname){
    const currentFile = pathname.split('/').pop() || '';
    if (currentFile.includes('.')) return pathname.replace(/[^/]*$/, '');
    return pathname.endsWith('/') ? pathname : `${pathname}/`;
  }

  function screenUrl(id){
    const url = new URL(window.location.href);
    url.pathname = `${screenBasePath(url.pathname)}${screenHref(id)}`;
    url.hash = '';
    return url;
  }

  function screenFromPath(){
    const currentFile = decodeURIComponent(window.location.pathname.split('/').pop() || 'index.html');
    if (!currentFile || !currentFile.includes('.')) return 'ob1';
    return pageScreens[currentFile] || '';
  }

  function screenFromLocation(){
    const hashId = window.location.hash.slice(1);
    if (hashId && document.getElementById(hashId)) return hashId;
    return screenFromPath() || 'ob1';
  }

  function updatePageUrl(id, replace = false){
    const nextUrl = screenUrl(id).toString();
    if (window.location.href === nextUrl) return;
    try {
      history[replace ? 'replaceState' : 'pushState'](null, '', nextUrl);
    } catch {
      window.location.href = nextUrl;
    }
  }

  function screenTitle(screen){
    const title = screen.querySelector('.shead h2')?.textContent?.trim();
    return title || screen.id;
  }

  function setDeckStatus(message){
    deckStatus.textContent = message;
    clearTimeout(setDeckStatus.timer);
    setDeckStatus.timer = setTimeout(() => { deckStatus.textContent = ''; }, 2200);
  }

  function currentScreen(){
    return document.querySelector('.screen.active') || screens[0];
  }

  function appNav(active){
    const items = [
      ['home', 'home', '⌂', 'Home'],
      ['sources', 'sources', '◫', 'Sources'],
      ['reviewcenter', 'review', '●', 'Review Center', '4'],
      ['settings', 'settings', '⚙', 'Settings']
    ];
    return `
      <nav class="app-global-nav" aria-label="App navigation">
        <div class="logo"><span class="aperture"></span>Locality</div>
        <button class="collapse-btn" type="button" aria-expanded="true" aria-label="Collapse sidebar">Collapse</button>
        ${items.map(([screenId, id, icon, label, count]) => `
          <a href="${screenHref(screenId)}" data-go="${screenId}" class="${active === id ? 'on' : ''}">
            <span class="nav-ico">${icon}</span><span class="nav-label">${label}</span>${count ? `<span class="cnt">${count}</span>` : ''}
          </a>
        `).join('')}
        <div class="foot"><span class="chip c-synced">✓</span> Synced · 2m ago</div>
      </nav>
    `;
  }

  function appParentLabel(active){
    return {
      home: 'Home',
      sources: 'Sources',
      review: 'Review Center',
      settings: 'Settings'
    }[active] || 'Home';
  }

  function screenForCrumb(label){
    return {
      Home: 'home',
      Sources: 'sources',
      'Review Center': 'reviewcenter',
      Settings: 'settings',
      'Files browser': 'files',
      'Locate results': 'locate',
      'Notion - Acme Team': 'srcdetail',
      'Push approval': 'review',
      'Sync problems': 'problems',
      'Conflict editor': 'conflict',
      Agents: 'agents',
      'Activity log': 'activity'
    }[label] || '';
  }

  function directElementAfterToolbar(frame){
    const toolbar = frame.querySelector(':scope > .tbar');
    let node = toolbar ? toolbar.nextElementSibling : frame.firstElementChild;
    return node || null;
  }

  function appendFrameBody(frame, page){
    const toolbar = frame.querySelector(':scope > .tbar');
    const first = toolbar ? toolbar.nextSibling : frame.firstChild;
    const nodes = [];
    for (let node = first; node; node = node.nextSibling) nodes.push(node);

    if (frame.dataset.stripLegacySidebar === 'true') {
      const legacyShell = directElementAfterToolbar(frame);
      if (legacyShell?.classList.contains('appshell')) {
        const legacyContent = Array.from(legacyShell.children).find(child => !child.classList.contains('snav'));
        if (legacyContent) {
          page.appendChild(legacyContent);
          legacyShell.remove();
          return;
        }
      }
    }

    nodes.forEach(node => page.appendChild(node));
  }

  function enhanceAppShells(){
    document.querySelectorAll('.frame[data-app-shell]').forEach(frame => {
      if (frame.dataset.shellEnhanced === 'true') return;
      const active = frame.dataset.appShell;
      const shell = document.createElement('div');
      shell.className = 'final-appshell';
      const page = document.createElement('div');
      page.className = 'app-page';

      if (frame.dataset.appKind) {
        const banner = document.createElement('div');
        banner.className = 'subpage-banner';
        const kind = frame.dataset.appKind === 'modal' ? 'Modal' : 'Subpage';
        const crumbs = (frame.dataset.appCrumbs || `${appParentLabel(active)}|${frame.dataset.appTitle || kind}`)
          .split('|')
          .map(part => part.trim())
          .filter(Boolean);
        const entry = frame.dataset.appEntry ? `<div class="entrypoints"><b>Reached from:</b> ${frame.dataset.appEntry}</div>` : '';
        banner.innerHTML = `
          <div class="crumbs" aria-label="Breadcrumbs">
            ${crumbs.map((crumb, index) => {
              const target = screenForCrumb(crumb);
              const current = index === crumbs.length - 1 ? ' aria-current="page"' : '';
              const href = target ? screenHref(target) : '#';
              return `${index ? '<span>/</span>' : ''}<a class="crumb-link" href="${href}"${target ? ` data-go="${target}"` : ''}${current}>${crumb}</a>`;
            }).join('')}
            <span class="chip ${frame.dataset.appKind === 'modal' ? 'c-review' : 'c-draft'}">${kind}</span>
          </div>
          ${entry}
        `;
        page.appendChild(banner);
      }

      appendFrameBody(frame, page);
      shell.innerHTML = appNav(active);
      shell.appendChild(page);
      frame.appendChild(shell);
      frame.dataset.shellEnhanced = 'true';
    });
  }

  function hydrateRouteLinks(){
    document.querySelectorAll('a[data-go]').forEach(link => {
      const target = link.dataset.go;
      if (target && screenPages[target]) {
        link.setAttribute('href', screenHref(target));
      }
    });
  }

  async function copyText(text){
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      const fallback = document.createElement('textarea');
      fallback.value = text;
      fallback.setAttribute('readonly', '');
      fallback.style.position = 'fixed';
      fallback.style.left = '-9999px';
      document.body.appendChild(fallback);
      fallback.select();
      document.execCommand('copy');
      fallback.remove();
    }
  }

  async function copyScreenLink(){
    const screen = currentScreen();
    await copyText(screenUrl(screen.id).toString());
    setDeckStatus(`copied link to ${screenTitle(screen)}`);
  }

  enhanceAppShells();
  hydrateRouteLinks();

  function show(id, options = {}){
    if (!document.getElementById(id)) return;
    screens.forEach(s => s.classList.toggle('active', s.id === id));
    nav.forEach(b => b.setAttribute('aria-current', b.dataset.s === id ? 'true' : 'false'));
    document.querySelector('.stage').scrollTop = 0;
    window.scrollTo({top:0});
    if (options.updateUrl !== false) {
      updatePageUrl(id, options.replaceUrl === true);
    }
    setDeckStatus('');
  }
  nav.forEach(b => b.addEventListener('click', e => {
    e.preventDefault();
    show(b.dataset.s);
  }));
  document.addEventListener('click', e => {
    const route = e.target.closest('[data-go]');
    if (route) {
      e.preventDefault();
      const target = route.dataset.go;
      if (target && document.getElementById(target)) {
        show(target);
        setDeckStatus(`opened ${screenTitle(document.getElementById(target))}`);
      }
      return;
    }

    const button = e.target.closest('.collapse-btn');
    if (!button) return;
    const shell = button.closest('.final-appshell');
    const collapsed = shell.classList.toggle('is-collapsed');
    button.textContent = collapsed ? 'Expand' : 'Collapse';
    button.setAttribute('aria-expanded', collapsed ? 'false' : 'true');
    button.setAttribute('aria-label', collapsed ? 'Expand sidebar' : 'Collapse sidebar');
  });
  document.getElementById('toggle-design-notes').addEventListener('click', e => {
    document.body.classList.toggle('hide-design-notes');
    e.currentTarget.textContent = document.body.classList.contains('hide-design-notes')
      ? 'Show design notes'
      : 'Hide design notes';
  });
  document.getElementById('copy-screen-link').addEventListener('click', copyScreenLink);
  window.addEventListener('popstate', () => {
    const id = screenFromLocation();
    show(document.getElementById(id) ? id : 'ob1', {updateUrl:false});
  });
  document.addEventListener('keydown', e => {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return;
    if (/input|textarea/i.test(document.activeElement.tagName)) return;
    const ids = [...nav].map(b => b.dataset.s);
    const cur = ids.indexOf(document.querySelector('.screen.active').id);
    const next = e.key === 'ArrowRight' ? Math.min(cur+1, ids.length-1) : Math.max(cur-1, 0);
    show(ids[next]);
  });
  const initialId = screenFromLocation();
  if (document.getElementById(initialId)) {
    show(initialId, {updateUrl:false});
  }
  if (window.location.hash && document.getElementById(initialId)) {
    updatePageUrl(initialId, true);
  }
