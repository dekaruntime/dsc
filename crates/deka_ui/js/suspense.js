// ui/suspense — boundary marker shared by server and (later) client.
// The renderer intercepts this tag before invoking it.

export function Suspense(props) {
  return props ? props.children : null;
}
Suspense.__dekaSuspense = true;
