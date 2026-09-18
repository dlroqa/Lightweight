import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import { createPortal } from "react-dom";

/**
 * A dropdown surface that cannot be seen through.
 *
 * The panel is required never to reproduce the readability defect where a
 * translucent menu let the composer, cards and page text show through and
 * collide with its own labels. This primitive is how that guarantee is kept in
 * one place:
 *
 * * It renders into `document.body` with a portal, so no ancestor's `opacity`,
 *   `overflow`, `transform` or stacking context reaches it. A decorative
 *   opacity anywhere up the tree cannot dim a menu that is not inside it.
 * * Its surface (`.menu`) paints a solid `background-color`, a solid `color`
 *   and an explicit `opacity: 1`, on a z-index above the composer, transcript,
 *   rail and sticky headers. The tokens it uses are opaque in both themes.
 * * The click-away shield is transparent and only captures the outside press;
 *   it dims nothing, because the menu is readable on its own rather than by
 *   virtue of the page behind it being darkened.
 *
 * Positioning is `fixed`, measured from the trigger's rect and re-measured on
 * scroll and resize, so the menu tracks its anchor without depending on an
 * offset parent.
 */
export function Menu({
  open,
  anchorRef,
  onClose,
  align = "start",
  minWidth,
  label,
  children,
}: {
  open: boolean;
  anchorRef: RefObject<HTMLElement | null>;
  onClose: () => void;
  /** Which edge of the trigger the menu lines up with. */
  align?: "start" | "end";
  minWidth?: number;
  label?: string;
  children: ReactNode;
}) {
  const menuRef = useRef<HTMLDivElement | null>(null);
  const [pos, setPos] = useState<{ top: number; left: number; width: number } | null>(null);

  const place = useCallback(() => {
    const anchor = anchorRef.current;
    if (!anchor) return;
    const rect = anchor.getBoundingClientRect();
    const margin = 8;
    // Open downward, but flip above the trigger when the menu would run off the
    // bottom and there is more room above — the composer control sits low, so
    // this is the common case there rather than the exception.
    const menuHeight = menuRef.current?.offsetHeight ?? 0;
    const below = window.innerHeight - rect.bottom - margin;
    const above = rect.top - margin;
    const flip = menuHeight > below && above > below;
    let top = flip ? rect.top - 6 - menuHeight : rect.bottom + 6;
    // Keep the whole menu on screen once its height is known: never let it run
    // off the bottom or above the top. `.menu`'s max-height keeps it shorter
    // than the viewport, so a clamped menu is always fully visible — and a
    // menu that never overflows the viewport cannot appear to "leak" the page
    // at points that fall off-screen.
    if (menuHeight > 0) {
      top = Math.min(top, window.innerHeight - margin - menuHeight);
      top = Math.max(margin, top);
    }
    setPos({ top, left: rect.left, width: rect.width });
  }, [anchorRef]);

  useLayoutEffect(() => {
    if (!open) return;
    place();
    // A second pass after the menu has painted, when its real height is known,
    // so the flip-above decision uses the true size rather than zero.
    const raf = requestAnimationFrame(place);
    window.addEventListener("resize", place);
    // Capture phase so a scroll in any container repositions the menu, not just
    // a scroll of the document itself.
    window.addEventListener("scroll", place, true);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open, place]);

  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        onClose();
        anchorRef.current?.focus();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open, onClose, anchorRef]);

  // Move focus into the menu when it opens, for keyboard reach.
  useEffect(() => {
    if (open && pos) {
      const first = menuRef.current?.querySelector<HTMLElement>(
        "[data-menu-item]:not([disabled])",
      );
      first?.focus();
    }
  }, [open, pos]);

  if (!open || pos === null) return null;

  const width = Math.max(minWidth ?? 0, pos.width);
  // Keep the menu on screen: if anchoring to the end would push it off the left
  // edge, or a wide menu would run off the right, clamp into the viewport.
  const rightAligned = align === "end";
  let left = rightAligned ? pos.left + pos.width - width : pos.left;
  const margin = 8;
  left = Math.min(left, window.innerWidth - width - margin);
  left = Math.max(margin, left);

  return createPortal(
    <>
      <button
        type="button"
        className="menu-shield"
        aria-hidden="true"
        tabIndex={-1}
        onClick={onClose}
      />
      <div
        ref={menuRef}
        className="menu"
        role="menu"
        aria-label={label}
        style={{ top: pos.top, left, minWidth: width }}
      >
        {children}
      </div>
    </>,
    document.body,
  );
}

/** One actionable row inside a {@link Menu}. */
export function MenuItem({
  onClick,
  disabled,
  children,
}: {
  onClick?: () => void;
  disabled?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      data-menu-item
      className="menu__item"
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  );
}
