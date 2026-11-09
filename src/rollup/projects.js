(() => {
  const form = document.querySelector('[data-project-lookup]');
  if (!form) return;
  const provider = form.elements.provider;
  const project = form.elements.project;
  const submit = form.querySelector('[type="submit"]');
  const refresh = form.querySelector('[data-refresh]');
  const status = form.querySelector('[role="status"]');
  const zh = document.documentElement.lang === 'zh';
  const pick = (en, cn) => zh ? cn : en;
  const selections = new Map([[provider.value, form.dataset.currentProject || '']]);
  let request = 0;
  let controller;

  async function load() {
    const version = ++request;
    if (controller) controller.abort();
    controller = new AbortController();
    const signal = controller.signal;
    const active = controller;
    const platform = provider.value;
    const selected = selections.get(platform) || '';
    const loading = pick('Discovering projects…', '正在检测已接入的仓库…');
    project.replaceChildren(new Option(loading, ''));
    project.disabled = submit.disabled = refresh.disabled = true;
    status.textContent = loading;
    const timeout = setTimeout(() => active.abort(), 25000);
    try {
      const response = await fetch(`/api/command-projects?provider=${encodeURIComponent(platform)}`, {
        cache: 'no-store', signal
      });
      if (!response.ok) throw new Error('discovery failed');
      const data = await response.json();
      if (data.provider !== platform || !Array.isArray(data.repositories) ||
          !data.repositories.every(name => typeof name === 'string')) {
        throw new Error('invalid directory');
      }
      if (version !== request) return;
      const names = data.repositories;
      const placeholder = new Option(names.length
        ? pick('Select a repository', '请选择仓库')
        : pick('No public projects found', '未发现可选的公开仓库'), '');
      placeholder.disabled = true;
      project.replaceChildren(placeholder);
      for (const name of names) project.add(new Option(name, name));
      const current = names.find(name => name.toLowerCase() === selected.toLowerCase());
      project.value = current || (names.length === 1 && !selected ? names[0] : '');
      if (project.value) selections.set(platform, project.value);
      project.disabled = !names.length;
      submit.disabled = !project.value;
      status.textContent = names.length
        ? pick(`Found ${names.length} connected public repositories.`, `已发现 ${names.length} 个已接入的公开仓库。`)
        : pick('No connected public repositories are available on this platform.', '此平台暂无已接入的公开仓库。');
      if (selected && !current) {
        status.textContent += pick(' The previous project is unavailable; select another.', ' 原先的仓库已不可选，请重新选择。');
      }
    } catch (_) {
      if (version !== request) return;
      project.replaceChildren(new Option(pick('Discovery unavailable', '暂时无法检测仓库'), ''));
      status.textContent = pick('Could not load projects. Retry shortly or use the manual form below.', '仓库列表加载失败，请稍后刷新，或使用下方的手动输入。');
    } finally {
      clearTimeout(timeout);
      if (version === request) refresh.disabled = false;
    }
  }
  project.addEventListener('change', () => {
    selections.set(provider.value, project.value);
    submit.disabled = !project.value;
  });
  provider.addEventListener('change', load);
  refresh.addEventListener('click', load);
  load();
})();
