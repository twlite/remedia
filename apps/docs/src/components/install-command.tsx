'use client';

import { useEffect, useState } from 'react';
import Link from 'next/link';
import { Check, Copy } from 'lucide-react';
import styles from './install-command.module.css';

type Target = 'node' | 'server';
type Platform = 'windows' | 'unix';

const nodeCommand = 'npm install @remedia/node';
const serverCommands: Record<Platform, string> = {
  windows: 'irm https://remedia.twlite.dev/install.ps1 | iex',
  unix: 'curl -fsSL https://remedia.twlite.dev/install.sh | sh',
};

export function InstallCommand() {
  const [target, setTarget] = useState<Target>('node');
  const [platform, setPlatform] = useState<Platform>('unix');
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    // eslint-disable-next-line react-hooks/set-state-in-effect
    if (/Windows/i.test(navigator.userAgent)) setPlatform('windows');
  }, []);

  const command = target === 'node' ? nodeCommand : serverCommands[platform];

  const selectTarget = (next: Target) => {
    setTarget(next);
    setCopied(false);
  };

  const selectPlatform = (next: Platform) => {
    setPlatform(next);
    setCopied(false);
  };

  return (
    <div className={styles.wrapper} aria-label="Install Remedia">
      <div className={styles.tabs} role="tablist" aria-label="What to install">
        <button
          type="button"
          role="tab"
          aria-selected={target === 'node'}
          onClick={() => selectTarget('node')}
        >
          Node.js package
        </button>
        <button
          type="button"
          role="tab"
          aria-selected={target === 'server'}
          onClick={() => selectTarget('server')}
        >
          Standalone server
        </button>
      </div>
      {target === 'server' ? (
        <div className={styles.tabs} role="tablist" aria-label="Operating system">
          <button
            type="button"
            role="tab"
            aria-selected={platform === 'unix'}
            onClick={() => selectPlatform('unix')}
          >
            macOS &amp; Linux
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={platform === 'windows'}
            onClick={() => selectPlatform('windows')}
          >
            Windows
          </button>
        </div>
      ) : null}
      <div className={styles.command}>
        <code>{command}</code>
        <button
          type="button"
          aria-label="Copy installation command"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(command);
              setCopied(true);
            } catch {
              setCopied(false);
            }
          }}
        >
          {copied ? <Check size={17} aria-hidden="true" /> : <Copy size={17} aria-hidden="true" />}
        </button>
      </div>
      <p className={styles.hint}>
        {target === 'node' ? (
          <>
            Runs the engine inside your bot. Also needs FFmpeg 9:{' '}
            <Link href="/docs/getting-started/embedded">embedded quickstart</Link>.
          </>
        ) : (
          <>
            Installs <code>remedia-server</code> and FFmpeg 9. Your bot connects with{' '}
            <code>@remedia/client</code>:{' '}
            <Link href="/docs/getting-started/remote">remote quickstart</Link>.
          </>
        )}
      </p>
    </div>
  );
}
