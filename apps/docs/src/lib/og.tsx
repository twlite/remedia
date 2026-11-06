import { ImageResponse } from 'next/og';

export const socialImageSize = { width: 1200, height: 630 };

export function createSocialImage({
  title,
  description,
  section = 'RUST AUDIO ENGINE',
  home = false,
}: {
  title: string;
  description?: string | undefined;
  section?: string;
  home?: boolean;
}) {
  return new ImageResponse(
    <div
      style={{
        display: 'flex',
        width: '100%',
        height: '100%',
        position: 'relative',
        overflow: 'hidden',
        backgroundColor: '#090b0e',
        color: '#f6f5fa',
        fontFamily: 'sans-serif',
        padding: 60,
        flexDirection: 'column',
        backgroundImage:
          'radial-gradient(ellipse at 100% 30%, #36214e 0%, #15101f 35%, #090b0e 75%)',
      }}
    >
      <div
        style={{
          display: 'flex',
          position: 'absolute',
          inset: 18,
          border: '1px solid #ffffff12',
          borderRadius: 22,
        }}
      />
      <div style={{ display: 'flex', alignItems: 'center', gap: 15 }}>
        <svg width="37" height="37" viewBox="0 0 36 36" fill="none">
          <path
            d="M5 15v6M11 9v18M18 3v30M25 10v16M31 15v6"
            stroke="#f6f5fa"
            strokeWidth="3"
            strokeLinecap="round"
          />
        </svg>
        <span style={{ fontSize: 29, fontWeight: 700, letterSpacing: -1 }}>Remedia</span>
      </div>
      <div
        style={{
          display: 'flex',
          position: 'absolute',
          right: 54,
          top: 143,
          height: 315,
          width: 292,
          alignItems: 'center',
          justifyContent: 'center',
          gap: 12,
          opacity: home ? 0.65 : 0.28,
        }}
      >
        {[52, 108, 170, 248, 312, 224, 150, 90, 42].map((height, index) => (
          <div
            key={index}
            style={{
              display: 'flex',
              width: 18,
              height,
              borderRadius: 14,
              backgroundImage: 'linear-gradient(180deg, #d8b4fe 0%, #a577df 45%, #62428c 100%)',
              boxShadow: '0 0 38px #a77af51a',
            }}
          />
        ))}
      </div>
      <div
        style={{
          display: 'flex',
          alignItems: 'center',
          gap: 10,
          alignSelf: 'flex-start',
          marginTop: 46,
          border: '1px solid #a78bfa50',
          borderRadius: 30,
          padding: '8px 15px',
          color: '#d0b9ed',
          fontSize: 14,
          letterSpacing: 2,
        }}
      >
        <span
          style={{
            display: 'flex',
            width: 7,
            height: 7,
            borderRadius: 10,
            backgroundColor: '#bb8ce9',
          }}
        />
        {section}
      </div>
      <div
        style={{
          display: 'flex',
          maxWidth: home ? 850 : 960,
          marginTop: 24,
          fontSize: home ? 68 : title.length > 55 ? 55 : 66,
          lineHeight: 1.1,
          letterSpacing: -2.5,
          fontWeight: 700,
          textWrap: 'balance',
        }}
      >
        {title}
      </div>
      <div
        style={{
          display: 'flex',
          maxWidth: 760,
          marginTop: 22,
          fontSize: 23,
          lineHeight: 1.45,
          color: '#a9a6b5',
        }}
      >
        {description && description.length > 160
          ? `${description.slice(0, 157).trimEnd()}…`
          : description}
      </div>
      <div
        style={{
          display: 'flex',
          position: 'absolute',
          left: 60,
          right: 60,
          bottom: 44,
          paddingTop: 22,
          borderTop: '1px solid #ffffff1a',
          alignItems: 'center',
          justifyContent: 'space-between',
        }}
      >
        <div style={{ display: 'flex', gap: 12, alignItems: 'center' }}>
          {(home
            ? ['Rust powered', 'Embedded + remote', 'Discord native']
            : ['Documentation', 'Native audio. One engine.']
          ).map((label) => (
            <span
              key={label}
              style={{
                borderRadius: 8,
                padding: '7px 12px',
                border: '1px solid #ffffff18',
                fontSize: 15,
                color: '#c9c6d0',
              }}
            >
              {label}
            </span>
          ))}
        </div>
        <span style={{ color: '#918a9e', fontSize: 17 }}>twlite/remedia ↗</span>
      </div>
    </div>,
    socialImageSize,
  );
}

export function createHomeSocialImage() {
  return createSocialImage({
    title: 'A modern audio engine for Discord.',
    description:
      'Native performance. Flexible deployment.\nOne Rust engine for embedded and remote audio.',
    home: true,
  });
}
