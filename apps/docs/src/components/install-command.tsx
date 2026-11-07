'use client';

import { useEffect, useState } from 'react';
import { Check, Copy } from 'lucide-react';
import styles from './install-command.module.css';

type Platform = 'windows' | 'unix';
const commands: Record<Platform, string> = {
  windows: 'irm https://raw.githubusercontent.com/twlite/remedia/main/scripts/install.ps1 | iex',
  unix: 'curl -fsSL https://raw.githubusercontent.com/twlite/remedia/main/scripts/install.sh | sh',
};

export function InstallCommand() {
  const [platform, setPlatform] = useState<Platform>('unix');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    if (/Windows/i.test(navigator.userAgent)) setPlatform('windows');
  }, []);

  const select = (next: Platform) => {
    setPlatform(next);
    setCopied(false);
  };

  return (
    <div className={styles.wrapper} aria-label="Install remedia-server">
      <div className={styles.tabs} role="tablist" aria-label="Operating system">
        <button
          type="button"
          role="tab"
          aria-selected={platform === 'windows'}
          onClick={() => select('windows')}
        >
          Windows
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={platform === 'unix'}
          onClick={() => select('unix')}
        >
          macOS &amp; Linux
        </button>
      </div>
      <div className={styles.command}>
        <code>{commands[platform]}</code>
        <button
          type="button"
          aria-label="Copy installation command"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(commands[platform]);
              setCopied(true);
            } catch {
              setCopied(false);
            }
          }}
        >
          {copied ? <Check size={17} aria-hidden="true" /> : <Copy size={17} aria-hidden="true" />}
        </button>
      </div>
    </div>
  );
}
