// Theme toggle. Without JS the page follows the system preference.
(function () {
  var root = document.documentElement;
  var btn = document.getElementById("theme-toggle");
  if (!btn) return;
  var media = window.matchMedia("(prefers-color-scheme: light)");
  function current() {
    return root.dataset.theme || (media.matches ? "light" : "dark");
  }
  function label() {
    var next = current() === "light" ? "dark" : "light";
    btn.setAttribute("aria-label", "Switch to " + next + " theme");
    btn.title = "Switch to " + next + " theme";
  }
  btn.hidden = false;
  label();
  media.addEventListener("change", label);
  btn.addEventListener("click", function () {
    var next = current() === "light" ? "dark" : "light";
    root.dataset.theme = next;
    try { localStorage.setItem("withnav-theme", next); } catch (e) {}
    label();
  });
})();
