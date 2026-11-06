'use client';

import { Search } from 'lucide-react';
import { useSearchContext } from 'fumadocs-ui/contexts/search';

export function HomeSearch({ className }: { className?: string | undefined }) {
  const { setOpenSearch } = useSearchContext();

  return (
    <button
      type="button"
      className={className}
      onClick={() => setOpenSearch(true)}
      aria-label="Search documentation"
    >
      <Search size={19} aria-hidden="true" />
      <span>Search docs...</span>
      <kbd aria-hidden="true">⌘ K</kbd>
    </button>
  );
}
