import { useEffect, useState } from "react";

const FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";

/** The braille spinner of the design (`⠹ running`). */
export function Spinner({ label = "running" }: { label?: string }) {
  const [i, setI] = useState(0);
  useEffect(() => {
    if (window.matchMedia?.("(prefers-reduced-motion: reduce)").matches) return;
    const timer = setInterval(() => setI((n) => (n + 1) % FRAMES.length), 90);
    return () => clearInterval(timer);
  }, []);
  return (
    <span className="spin" role="img" aria-label={label}>
      {FRAMES[i]}
    </span>
  );
}
