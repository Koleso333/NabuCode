import { createSignal } from "solid-js";

// The one error toast of the window
const [error, setError] = createSignal<string | null>(null);
export { error, setError };

export const showFailure = (cause: unknown) => setError(cause instanceof Error ? cause.message : String(cause));
