// Day/Night theme for xerj.org. Loaded blocking in <head>, before style.css,
// so a remembered choice applies before the first paint instead of flashing
// the default "day". The choice lives in localStorage and is read on every
// page; the DAY/NIGHT buttons (button[data-theme]) write it. See #1973.
(function () {
  var KEY = 'xerj.theme';
  var root = document.documentElement;
  var saved = null;
  try { saved = localStorage.getItem(KEY); } catch (_) {}
  if (saved === 'day' || saved === 'night') root.setAttribute('data-theme', saved);

  function buttons() { return document.querySelectorAll('button[data-theme]'); }
  function apply(theme) {
    root.setAttribute('data-theme', theme);
    var all = buttons();
    for (var i = 0; i < all.length; i++) {
      all[i].classList.toggle('active', all[i].getAttribute('data-theme') === theme);
    }
  }
  function onClick() {
    var theme = this.getAttribute('data-theme');
    apply(theme);
    try { localStorage.setItem(KEY, theme); } catch (_) {}
  }
  document.addEventListener('DOMContentLoaded', function () {
    apply(root.getAttribute('data-theme') || 'day');
    var all = buttons();
    for (var i = 0; i < all.length; i++) all[i].addEventListener('click', onClick);
  });
})();
