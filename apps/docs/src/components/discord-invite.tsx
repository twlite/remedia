import Image from 'next/image';
import { discordInviteUrl } from '@/lib/shared';
import styles from './discord-invite.module.css';

/**
 * Invites visitors to the AndrozDev support community shared with Discord Player.
 * @returns The support server card and invitation links.
 */
export function DiscordInvite() {
  return (
    <section className={styles.card} aria-labelledby="discord-invite-heading">
      <p className={styles.eyebrow}>Join our official support server</p>
      <div className={styles.content}>
        <div className={styles.server}>
          <Image
            src={`${process.env.NEXT_PUBLIC_BASE_PATH ?? ''}/androzdev.webp`}
            alt=""
            width={56}
            height={56}
            className={styles.icon}
            unoptimized
          />
          <div>
            <h2 id="discord-invite-heading" className={styles.name}>
              <a href={discordInviteUrl} target="_blank" rel="noopener noreferrer">
                AndrozDev
              </a>
            </h2>
            <p className={styles.description}>Get help and share what you build.</p>
          </div>
        </div>
        <a
          href={discordInviteUrl}
          target="_blank"
          rel="noopener noreferrer"
          className={styles.join}
        >
          Join server
        </a>
      </div>
    </section>
  );
}
