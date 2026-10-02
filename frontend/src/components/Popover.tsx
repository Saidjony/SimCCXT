import { useEffect, useRef, useState } from "react";
import type { ReactNode } from "react";

/**
 * Popover for the topbar.
 *
 * The topbar is a single row that scrolls horizontally (`overflow-x: auto`),
 * which makes it a clipping container: a menu positioned `absolute` inside it
 * is cut off at the 40px topbar height and never becomes visible. Fixed
 * positioning with coordinates taken from the trigger escapes that clip, so
 * the menu always opens below the button (flipped to stay on screen).
 */
export function Popover(props: {
  /** Button content. */
  label: ReactNode;
  /** Plain-text label for assistive tech. */
  ariaLabel: string;
  title?: string;
  /** Class of the menu panel (layout/skin lives in styles.css). */
  panelClass: string;
  /** Menu width in px; the panel is clamped to the viewport. */
  width?: number;
  children: ReactNode;
}): JSX.Element {
  const { label, ariaLabel, title, panelClass, width = 260, children } = props;
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const panelRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState({ top: 0, left: 0 });

  useEffect(() => {
    if (!open) return;
    const place = (): void => {
      const box = triggerRef.current?.getBoundingClientRect();
      if (!box) return;
      const panelWidth = Math.min(width, window.innerWidth - 16);
      // Flip to the button's left edge when the menu would leave the window.
      const left = Math.max(8, Math.min(box.right - panelWidth, window.innerWidth - panelWidth - 8));
      setPos({ top: box.bottom + 6, left });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open, width]);

  useEffect(() => {
    if (!open) return;
    const onDown = (event: MouseEvent): void => {
      const target = event.target as Node | null;
      if (panelRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (event: KeyboardEvent): void => {
      if (event.key === "Escape") setOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <>
      <button
        ref={triggerRef}
        className="topbar-pop-btn"
        aria-label={ariaLabel}
        aria-expanded={open}
        title={title}
        onClick={() => setOpen((value) => !value)}
      >
        {label}
      </button>
      {open && (
        <div
          ref={panelRef}
          className={"topbar-pop " + panelClass}
          style={{ top: pos.top, left: pos.left, width }}
        >
          {children}
        </div>
      )}
    </>
  );
}
