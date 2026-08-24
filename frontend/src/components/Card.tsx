import type { ReactNode } from "react";

interface CardProps {
  title?: ReactNode;
  action?: ReactNode;
  children: ReactNode;
  flush?: boolean;
  className?: string;
}

export function Card({ title, action, children, flush, className }: CardProps) {
  return (
    <section
      className={`card${flush ? " card--flush" : ""}${className ? ` ${className}` : ""}`}
    >
      {(title || action) && (
        <header className="card__head" style={flush ? { padding: "20px 20px 0" } : undefined}>
          {title ? <h2 className="card__title">{title}</h2> : <span />}
          {action}
        </header>
      )}
      {children}
    </section>
  );
}
