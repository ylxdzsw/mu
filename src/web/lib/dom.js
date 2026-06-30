import { iconMarkup } from "./icons.js";

export function clear(node) {
  node.replaceChildren();
}

export function element(tagName, className, textContent) {
  const node = document.createElement(tagName);
  if (className) {
    node.className = className;
  }
  if (textContent != null) {
    node.textContent = textContent;
  }
  return node;
}

export function button(className, label) {
  const node = element("button", className);
  node.type = "button";
  if (label) {
    node.setAttribute("aria-label", label);
    node.title = label;
  }
  return node;
}

export function resetSelectOptions(select, options) {
  select.replaceChildren();
  for (const option of options) {
    const node = document.createElement("option");
    node.value = option.value;
    node.textContent = option.label;
    select.appendChild(node);
  }
}

export function setIcon(node, name) {
  node.innerHTML = iconMarkup(name);
}
