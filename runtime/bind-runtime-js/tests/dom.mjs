// A minimal DOM for the runtime tests: just what `render` may use (createElement,
// createTextNode, setAttribute, appendChild, replaceChildren, textContent) plus a serializer
// with exactly the escaping of the Rust runtime (`escape_html`: & < > " ' in text and in
// attribute values). Any use of an HTML-string API (innerHTML, outerHTML assignment,
// insertAdjacentHTML, document.write) is recorded in `document.htmlApiUsed` and throws.

const VOID = new Set(["hr", "br", "img", "meta", "link", "input"]);

export function escapeHtml(text) {
  let out = "";
  for (const c of text) {
    switch (c) {
      case "&": out += "&amp;"; break;
      case "<": out += "&lt;"; break;
      case ">": out += "&gt;"; break;
      case '"': out += "&quot;"; break;
      case "'": out += "&#39;"; break;
      default: out += c;
    }
  }
  return out;
}

function htmlApi(document, name) {
  document.htmlApiUsed.push(name);
  throw new Error(`HTML string API used: ${name}`);
}

class TextNode {
  constructor(document, data) {
    this.ownerDocument = document;
    this.nodeType = 3;
    this.data = String(data);
    this.parentNode = null;
  }
  get textContent() { return this.data; }
  set textContent(value) { this.data = String(value); }
}

class Element {
  constructor(document, tagName) {
    this.ownerDocument = document;
    this.nodeType = 1;
    this.tagName = String(tagName).toLowerCase();
    this.attributes = []; // [name, value] in insertion order, like a real element
    this.childNodes = [];
    this.parentNode = null;
  }
  setAttribute(name, value) {
    name = String(name).toLowerCase();
    value = String(value);
    const existing = this.attributes.find((attribute) => attribute[0] === name);
    if (existing) existing[1] = value;
    else this.attributes.push([name, value]);
  }
  getAttribute(name) {
    const existing = this.attributes.find((attribute) => attribute[0] === name);
    return existing ? existing[1] : null;
  }
  appendChild(child) {
    if (!(child instanceof Element || child instanceof TextNode)) {
      throw new TypeError("appendChild: not a node");
    }
    if (child.parentNode) {
      const siblings = child.parentNode.childNodes;
      siblings.splice(siblings.indexOf(child), 1);
    }
    child.parentNode = this;
    this.childNodes.push(child);
    return child;
  }
  replaceChildren(...nodes) {
    for (const child of this.childNodes) child.parentNode = null;
    this.childNodes = [];
    for (const node of nodes) {
      this.appendChild(typeof node === "string" ? new TextNode(this.ownerDocument, node) : node);
    }
  }
  get textContent() {
    return this.childNodes.map((child) => child.textContent).join("");
  }
  set textContent(value) {
    this.replaceChildren(new TextNode(this.ownerDocument, value));
  }
  get outerHTML() { return serialize(this); }
  set outerHTML(_value) { htmlApi(this.ownerDocument, "outerHTML"); }
  get innerHTML() { return this.childNodes.map(serialize).join(""); }
  set innerHTML(_value) { htmlApi(this.ownerDocument, "innerHTML"); }
  insertAdjacentHTML() { htmlApi(this.ownerDocument, "insertAdjacentHTML"); }
}

/** The HTML of `root`, iteratively (a tree of any depth serializes without a RangeError). */
export function serialize(root) {
  const parts = [];
  const stack = [root];
  while (stack.length > 0) {
    const node = stack.pop();
    if (typeof node === "string") {
      parts.push(node); // an end tag
      continue;
    }
    if (node instanceof TextNode) {
      parts.push(escapeHtml(node.data));
      continue;
    }
    let open = `<${node.tagName}`;
    for (const [name, value] of node.attributes) open += ` ${name}="${escapeHtml(value)}"`;
    parts.push(`${open}>`);
    if (VOID.has(node.tagName)) {
      if (node.childNodes.length > 0) throw new Error(`<${node.tagName}> with children`);
      continue;
    }
    stack.push(`</${node.tagName}>`);
    for (let i = node.childNodes.length - 1; i >= 0; i--) stack.push(node.childNodes[i]);
  }
  return parts.join("");
}

export function createDocument() {
  const document = {
    htmlApiUsed: [],
    createElement(tagName) { return new Element(document, tagName); },
    createTextNode(data) { return new TextNode(document, data); },
    write() { htmlApi(document, "document.write"); },
    writeln() { htmlApi(document, "document.writeln"); },
  };
  document.body = new Element(document, "body");
  return document;
}
