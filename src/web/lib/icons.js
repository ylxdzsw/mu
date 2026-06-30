export function iconMarkup(name) {
  switch (name) {
    case "plus":
      return `
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="M8 3v10M3 8h10" fill="none" stroke="currentColor" stroke-linecap="round" stroke-width="1.25"/>
        </svg>
      `;
    case "globe":
      return `
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="M8 2.25a5.75 5.75 0 1 0 0 11.5 5.75 5.75 0 0 0 0-11.5Zm0 0c1.48 0 2.68 2.57 2.68 5.75S9.48 13.75 8 13.75 5.32 11.18 5.32 8 6.52 2.25 8 2.25Zm-5.18 4.9h10.36M2.82 8.85h10.36"
            fill="none" stroke="currentColor" stroke-linecap="round" stroke-width="1.1"/>
        </svg>
      `;
    case "edit":
      return `
        <svg viewBox="0 0 16 16" aria-hidden="true">
          <path d="M3.5 12.5h1.7L12 5.7 10.3 4 3.5 10.8v1.7ZM9.8 4.5l1.7 1.7"
            fill="none" stroke="currentColor" stroke-linecap="round" stroke-linejoin="round" stroke-width="1.2"/>
        </svg>
      `;
    default:
      return "";
  }
}
