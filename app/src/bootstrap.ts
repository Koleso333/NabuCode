document.addEventListener("contextmenu", event => event.preventDefault());

window.addEventListener("load", () => {
  void import("./main");
}, { once: true });
