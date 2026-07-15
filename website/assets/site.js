document.documentElement.classList.add("js");

const header = document.querySelector("[data-header]");
const nav = document.querySelector("[data-nav]");
const navToggle = document.querySelector("[data-nav-toggle]");

const updateHeader = () => header?.classList.toggle("scrolled", window.scrollY > 12);
updateHeader();
window.addEventListener("scroll", updateHeader, { passive: true });

const closeNav = () => {
  nav?.classList.remove("open");
  navToggle?.setAttribute("aria-expanded", "false");
};

navToggle?.addEventListener("click", () => {
  const open = !nav?.classList.contains("open");
  nav?.classList.toggle("open", open);
  navToggle.setAttribute("aria-expanded", String(open));
});

nav?.querySelectorAll("a").forEach((link) => link.addEventListener("click", closeNav));
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeNav();
});

const installCommands = [
  {
    label: "Cargo",
    value: "cargo install vmux-tui",
  },
  {
    label: "shell",
    value: "curl -fsSL https://raw.githubusercontent.com/UAEpro/vmux/main/install.sh | sh",
  },
];

document.querySelectorAll("[data-install-rotator]").forEach((rotator) => {
  const command = rotator.querySelector("[data-install-command]");
  const copyButton = rotator.querySelector("[data-copy]");
  let current = 0;

  window.setInterval(() => {
    rotator.classList.add("switching");
    window.setTimeout(() => {
      current = (current + 1) % installCommands.length;
      const next = installCommands[current];
      command.textContent = next.value;
      command.title = next.value;
      copyButton.dataset.copy = next.value;
      copyButton.setAttribute("aria-label", `Copy ${next.label} install command`);
      rotator.classList.remove("switching");
    }, 180);
  }, 4200);
});

document.querySelectorAll("[data-copy]").forEach((button) => {
  button.addEventListener("click", async () => {
    const value = button.dataset.copy || "";
    const showCopied = () => {
      const wrap = button.closest("[data-copy-wrap]");
      wrap?.classList.add("copied");
      window.setTimeout(() => wrap?.classList.remove("copied"), 1600);
    };

    try {
      await navigator.clipboard.writeText(value);
      showCopied();
    } catch {
      const input = document.createElement("textarea");
      input.value = value;
      input.setAttribute("readonly", "");
      input.style.position = "fixed";
      input.style.opacity = "0";
      document.body.append(input);
      input.select();
      const copied = document.execCommand("copy");
      input.remove();
      if (copied) showCopied();
      else button.setAttribute("aria-label", "Copy failed—select the command manually");
    }
  });
});

const reveals = document.querySelectorAll(".reveal");
if ("IntersectionObserver" in window) {
  const observer = new IntersectionObserver(
    (entries) => {
      entries.forEach((entry) => {
        if (!entry.isIntersecting) return;
        entry.target.classList.add("visible");
        observer.unobserve(entry.target);
      });
    },
    { rootMargin: "0px 0px -8%", threshold: 0.08 },
  );
  reveals.forEach((item) => observer.observe(item));
} else {
  reveals.forEach((item) => item.classList.add("visible"));
}
