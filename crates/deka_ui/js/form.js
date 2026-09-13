// ui/form — progressive enhancement. Renders a plain HTML <form>. Zero JS.

import { createComponentNode } from "./node.js";

export function Form(props) {
  return createComponentNode("form", props);
}
