import Link from 'next/link';
import {
  ArrowRight,
  AudioLines,
  ChevronDown,
  CodeXml,
  ExternalLink,
  Layers,
  LayoutGrid,
  Zap,
} from 'lucide-react';
import { HomeSearch } from '@/components/home-search';
import { InstallCommand } from '@/components/install-command';
import { version } from '../../../../../packages/remedia-node/package.json';
import styles from './page.module.css';

function GitHubMark({ size = 24 }: { size?: number }) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
      <path d="M12 .75a11.25 11.25 0 0 0-3.558 21.922c.563.104.768-.244.768-.542 0-.267-.01-.974-.015-1.912-3.13.68-3.79-1.51-3.79-1.51-.512-1.3-1.25-1.646-1.25-1.646-1.023-.7.078-.686.078-.686 1.13.08 1.725 1.16 1.725 1.16 1.006 1.724 2.64 1.226 3.282.937.102-.728.394-1.226.716-1.508-2.499-.284-5.126-1.25-5.126-5.563 0-1.229.44-2.233 1.16-3.02-.116-.284-.503-1.43.11-2.98 0 0 .945-.303 3.094 1.154A10.79 10.79 0 0 1 12 6.177c.956.005 1.918.13 2.816.379 2.148-1.457 3.091-1.153 3.091-1.153.615 1.549.228 2.695.112 2.98.722.786 1.158 1.79 1.158 3.019 0 4.324-2.631 5.275-5.138 5.553.404.35.764 1.042.764 2.1 0 1.516-.014 2.74-.014 3.111 0 .3.202.65.774.54A11.252 11.252 0 0 0 12 .75Z" />
    </svg>
  );
}

export const metadata = {
  title: 'Remedia — A modern audio engine for Discord',
  description:
    'A Rust-first audio engine for embedded and remote playback, with native Discord voice integration and a typed API.',
};

const githubUrl = 'https://github.com/twlite/remedia';
const features = [
  {
    icon: Zap,
    title: 'High performance',
    description: 'Native audio processing, built in Rust from the ground up.',
  },
  {
    icon: Layers,
    title: 'Flexible deployment',
    description: 'Run embedded in your app or as a remote server.',
  },
  {
    icon: CodeXml,
    title: 'Discord native',
    description: 'Built for Discord voice with a clean, typed API.',
  },
  {
    icon: LayoutGrid,
    title: 'Extensible',
    description: 'Bring your own sources, with native filters and more.',
  },
];

const code = [
  <>
    <span className={styles.keyword}>import</span> {'{ Remedia }'}{' '}
    <span className={styles.keyword}>from</span>{' '}
    <span className={styles.string}>'@remedia/node'</span>;
  </>,
  '',
  <>
    <span className={styles.keyword}>const</span> remedia ={' '}
    <span className={styles.keyword}>await</span> <span className={styles.type}>Remedia</span>
    .create();
  </>,
  '',
  <>
    <span className={styles.keyword}>const</span> info ={' '}
    <span className={styles.keyword}>await</span> remedia.probe({'{'}
  </>,
  <>
    {' '}
    url: <span className={styles.string}>'https://example.com/track.mp3'</span>,
  </>,
  '});',
  '',
  <>console.log(info.durationMs);</>,
  <>
    <span className={styles.keyword}>await</span> remedia.close();
  </>,
];

export default function HomePage() {
  return (
    <div className={styles.landing}>
      <header className={styles.header}>
        <div className={styles.headerInner}>
          <Link href="/" className={styles.brand} aria-label="Remedia home">
            <AudioLines size={30} strokeWidth={2} aria-hidden="true" />
            <span>Remedia</span>
          </Link>
          <nav className={styles.navigation} aria-label="Main navigation">
            <Link href="/docs">Docs</Link>
            <Link href="/docs/node/bindings">API</Link>
            <Link href="/docs/client/overview">Guides</Link>
            <Link href="/docs/discord/bot-harness">Examples</Link>
          </nav>
          <div className={styles.headerActions}>
            <HomeSearch className={styles.search} />
            <a
              href={githubUrl}
              className={styles.githubIcon}
              aria-label="Remedia on GitHub"
              target="_blank"
              rel="noreferrer"
            >
              <GitHubMark size={24} aria-hidden="true" />
            </a>
            <details className={styles.version}>
              <summary>
                v{version}
                <ChevronDown size={14} aria-hidden="true" />
              </summary>
              <div className={styles.versionMenu}>
                <Link href="/docs">v{version} documentation</Link>
                <a href={`${githubUrl}/releases`}>
                  All releases <ExternalLink size={12} aria-hidden="true" />
                </a>
              </div>
            </details>
          </div>
        </div>
      </header>

      <main className={styles.main}>
        <section className={styles.hero} aria-labelledby="hero-title">
          <div className={styles.heroCopy}>
            <div className={styles.badge}>
              <span />
              Rust audio engine for Discord
            </div>
            <h1 id="hero-title">
              <span className={styles.product}>Remedia</span>
              <span className={styles.subtitle}>
                A modern audio engine
                <br className={styles.desktopBreak} /> for Discord.
              </span>
            </h1>
            <p className={styles.description}>
              High performance, low overhead audio playback with a unified engine for both embedded
              and remote setups. Built in Rust, designed for discord-player and beyond.
            </p>
            <InstallCommand />
            <div className={styles.buttons}>
              <Link href="/docs" className={styles.primaryButton}>
                Get Started <ArrowRight size={19} aria-hidden="true" />
              </Link>
              <a
                href={githubUrl}
                className={styles.secondaryButton}
                target="_blank"
                rel="noreferrer"
              >
                <GitHubMark size={20} aria-hidden="true" />
                View on GitHub
              </a>
            </div>
          </div>
          <div className={styles.codeWindow}>
            <div className={styles.codeHeader}>
              <div className={styles.windowDots} aria-hidden="true">
                <i />
                <i />
                <i />
              </div>
              <span>index.ts</span>
            </div>
            <pre
              className={styles.code}
              aria-label="Probe audio metadata with the native Remedia API"
            >
              <code>
                {code.map((line, index) => (
                  <span className={styles.codeLine} key={index}>
                    <span className={styles.lineNumber} aria-hidden="true">
                      {index + 1}
                    </span>
                    <span>{line || ' '}</span>
                    {'\n'}
                  </span>
                ))}
              </code>
            </pre>
          </div>
        </section>

        <section className={styles.features} aria-label="Why Remedia">
          {features.map(({ icon: Icon, title, description }) => (
            <div key={title} className={styles.feature}>
              <div className={styles.featureIcon}>
                <Icon size={25} strokeWidth={1.8} aria-hidden="true" />
              </div>
              <div>
                <h2>{title}</h2>
                <p>{description}</p>
              </div>
            </div>
          ))}
        </section>
      </main>

      <footer className={styles.footer}>
        <p>Powering the next generation of Discord audio, including</p>
        <a href="https://discord-player.js.org" target="_blank" rel="noreferrer">
          discord-player <ExternalLink size={13} aria-hidden="true" />
        </a>
      </footer>
    </div>
  );
}
