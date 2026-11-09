(() => {
  const form = document.querySelector('[data-project-lookup]');
  if (!form) return;
  const provider = form.elements.provider;
  const project = form.elements.project;
  const platforms = [...form.querySelectorAll('[data-provider]')];
  const picker = form.querySelector('[data-project-picker]');
  const trigger = picker.querySelector('[data-project-trigger]');
  const value = picker.querySelector('[data-project-name]');
  const popup = picker.querySelector('.project-popover');
  const search = picker.querySelector('[data-project-search]');
  const clear = picker.querySelector('[data-project-clear]');
  const list = picker.querySelector('[role="listbox"]');
  const empty = picker.querySelector('[data-project-empty]');
  const results = picker.querySelector('[data-project-results]');
  const submit = form.querySelector('[type="submit"]');
  const refresh = form.querySelector('[data-refresh]');
  const status = form.querySelector('.project-status');
  const zh = document.documentElement.lang === 'zh';
  const pick = (en, cn) => zh ? cn : en;
  const selections = new Map([[provider.value, form.dataset.currentProject || '']]);
  let names = [];
  let visible = [];
  let activeIndex = -1;
  let request = 0;
  let controller;

  function position() {
    if (popup.hidden) return;
    const rect = trigger.getBoundingClientRect();
    const viewport = window.visualViewport;
    const top = viewport ? viewport.offsetTop : 0;
    const bottom = top + (viewport ? viewport.height : innerHeight);
    const below = bottom - rect.bottom - 16;
    const above = rect.top - top - 16;
    const upwards = below < 260 && above > below;
    popup.dataset.side = upwards ? 'above' : 'below';
    popup.style.setProperty('--project-list-height', `${Math.max(72, Math.min(300, (upwards ? above : below) - 100))}px`);
  }
  function close(restoreFocus = false) {
    popup.hidden = true;
    trigger.setAttribute('aria-expanded', 'false');
    search.setAttribute('aria-expanded', 'false');
    search.removeAttribute('aria-activedescendant');
    if (restoreFocus) trigger.focus();
  }
  function activate(index, scroll = true) {
    activeIndex = visible.length ? (index + visible.length) % visible.length : -1;
    [...list.children].forEach((option, i) => option.classList.toggle('is-active', i === activeIndex));
    const option = list.children[activeIndex];
    if (!option) {
      search.removeAttribute('aria-activedescendant');
      return;
    }
    search.setAttribute('aria-activedescendant', option.id);
    if (scroll) {
      if (option.offsetTop < list.scrollTop) list.scrollTop = option.offsetTop;
      else if (option.offsetTop + option.offsetHeight > list.scrollTop + list.clientHeight) {
        list.scrollTop = option.offsetTop + option.offsetHeight - list.clientHeight;
      }
    }
  }
  function choose(name) {
    if (!names.includes(name)) return;
    project.value = name;
    value.textContent = name;
    trigger.classList.add('has-selection');
    selections.set(provider.value, name);
    submit.disabled = false;
    close(true);
  }
  function render() {
    const terms = search.value.trim().toLowerCase().split(/\s+/).filter(Boolean);
    visible = names.filter(name => terms.every(term => name.toLowerCase().includes(term)));
    if (terms.length) {
      const rank = name => {
        const repository = name.slice(name.indexOf('/') + 1).toLowerCase();
        return terms.reduce((score, term) => score + (repository.startsWith(term) ? 0 : repository.includes(term) ? 1 : 2), 0);
      };
      visible.sort((a, b) => rank(a) - rank(b));
    }
    const fragment = document.createDocumentFragment();
    visible.forEach((name, index) => {
      const option = document.createElement('div');
      option.id = `project-option-${index}`;
      option.className = 'project-option';
      option.setAttribute('role', 'option');
      option.setAttribute('aria-selected', String(name === project.value));
      option.setAttribute('aria-label', name);
      const icon = trigger.querySelector('.repo-icon').cloneNode(true);
      const text = document.createElement('span');
      text.className = 'project-option-text';
      const repository = document.createElement('span');
      repository.className = 'project-option-name';
      repository.textContent = name.slice(name.indexOf('/') + 1);
      const owner = document.createElement('span');
      owner.className = 'project-option-owner';
      owner.textContent = name.slice(0, name.indexOf('/'));
      text.append(repository, owner);
      const tick = document.createElement('span');
      tick.className = 'project-option-tick';
      tick.textContent = name === project.value ? '✓' : '';
      tick.setAttribute('aria-hidden', 'true');
      option.append(icon, text, tick);
      option.addEventListener('pointermove', () => activate(index, false));
      option.addEventListener('mousedown', event => event.preventDefault());
      option.addEventListener('click', () => choose(name));
      fragment.append(option);
    });
    list.replaceChildren(fragment);
    list.scrollTop = 0;
    empty.hidden = !!visible.length;
    clear.hidden = !search.value;
    results.textContent = terms.length
      ? pick(`${visible.length} matches · ${names.length} repositories`, `${visible.length} 个匹配 · 共 ${names.length} 个仓库`)
      : pick(`${names.length} repositories · ↑↓ to navigate, Enter to select`, `${names.length} 个仓库 · ↑↓ 切换，回车选择`);
    position();
    activate(Math.max(0, visible.indexOf(project.value)));
  }
  function open(last = false) {
    if (trigger.disabled) return;
    popup.hidden = false;
    trigger.setAttribute('aria-expanded', 'true');
    search.setAttribute('aria-expanded', 'true');
    search.value = '';
    render();
    if (last && !project.value) activate(visible.length - 1);
    search.focus({preventScroll: true});
  }
  async function load() {
    const version = ++request;
    if (controller) controller.abort();
    controller = new AbortController();
    const active = controller;
    const platform = provider.value;
    const selected = selections.get(platform) || '';
    const loading = pick('Discovering projects…', '正在检测已接入的仓库…');
    close();
    names = visible = [];
    list.replaceChildren();
    project.value = '';
    value.textContent = loading;
    trigger.classList.remove('has-selection');
    trigger.disabled = submit.disabled = refresh.disabled = true;
    status.textContent = loading;
    const timeout = setTimeout(() => active.abort(), 25000);
    try {
      const response = await fetch(`/api/command-projects?provider=${encodeURIComponent(platform)}`, {
        cache: 'no-store', signal: active.signal
      });
      if (!response.ok) throw new Error('discovery failed');
      const data = await response.json();
      if (data.provider !== platform || !Array.isArray(data.repositories) ||
          !data.repositories.every(name => typeof name === 'string')) {
        throw new Error('invalid directory');
      }
      if (version !== request) return;
      names = data.repositories;
      const current = names.find(name => name.toLowerCase() === selected.toLowerCase());
      project.value = current || (names.length === 1 && !selected ? names[0] : '');
      if (project.value) selections.set(platform, project.value);
      value.textContent = project.value || (names.length
        ? pick('Select a repository', '请选择仓库')
        : pick('No public projects found', '未发现可选的公开仓库'));
      trigger.classList.toggle('has-selection', !!project.value);
      trigger.disabled = !names.length;
      submit.disabled = !project.value;
      status.textContent = names.length
        ? pick(`Found ${names.length} connected public repositories.`, `已发现 ${names.length} 个已接入的公开仓库。`)
        : pick('No connected public repositories are available on this platform.', '此平台暂无已接入的公开仓库。');
      if (selected && !current) {
        status.textContent += pick(' The previous project is unavailable; select another.', ' 原先的仓库已不可选，请重新选择。');
      }
    } catch (_) {
      if (version !== request) return;
      value.textContent = pick('Discovery unavailable', '暂时无法检测仓库');
      status.textContent = pick('Could not load projects. Retry shortly or use the manual form below.', '仓库列表加载失败，请稍后刷新，或使用下方的手动输入。');
    } finally {
      clearTimeout(timeout);
      if (version === request) refresh.disabled = false;
    }
  }
  function platform(button) {
    if (provider.value === button.dataset.provider) return;
    provider.value = button.dataset.provider;
    platforms.forEach(item => {
      const checked = item === button;
      item.setAttribute('aria-checked', String(checked));
      item.tabIndex = checked ? 0 : -1;
    });
    load();
  }
  platforms.forEach((button, index) => {
    button.addEventListener('click', () => platform(button));
    button.addEventListener('keydown', event => {
      if (!['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End'].includes(event.key)) return;
      event.preventDefault();
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? platforms.length - 1
        : (index + (['ArrowLeft', 'ArrowUp'].includes(event.key) ? -1 : 1) + platforms.length) % platforms.length;
      platforms[next].focus();
      platform(platforms[next]);
    });
  });
  trigger.addEventListener('click', () => popup.hidden ? open() : close());
  trigger.addEventListener('keydown', event => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      open(event.key === 'ArrowUp');
    }
  });
  search.addEventListener('input', render);
  search.addEventListener('keydown', event => {
    if (event.isComposing) return;
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      activate(activeIndex + (event.key === 'ArrowDown' ? 1 : -1));
    } else if (event.key === 'Enter') {
      event.preventDefault();
      if (visible[activeIndex]) choose(visible[activeIndex]);
    }
  });
  clear.addEventListener('click', () => { search.value = ''; render(); search.focus(); });
  picker.addEventListener('keydown', event => {
    if (event.isComposing) return;
    if (event.key === 'Escape' && !popup.hidden) {
      event.preventDefault();
      event.stopPropagation();
      close(true);
    }
  });
  picker.addEventListener('focusout', () => queueMicrotask(() => {
    if (!picker.contains(document.activeElement)) close();
  }));
  document.addEventListener('pointerdown', event => { if (!picker.contains(event.target)) close(); });
  window.addEventListener('resize', position);
  window.visualViewport?.addEventListener('resize', position);
  document.addEventListener('scroll', position, true);
  form.addEventListener('submit', event => {
    if (!names.includes(project.value)) { event.preventDefault(); open(); }
  });
  refresh.addEventListener('click', load);
  load();
})();
