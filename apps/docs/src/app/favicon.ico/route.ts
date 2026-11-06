export const revalidate = false;

export async function GET() {
  return new Response(
    '<svg xmlns="http://www.w3.org/2000/svg" width="30" height="30" viewBox="0 0 24 24" fill="none" stroke="#FFFFFF" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" class="lucide lucide-audio-lines" aria-hidden="true"><path d="M2 10v3"></path><path d="M6 6v11"></path><path d="M10 3v18"></path><path d="M14 8v7"></path><path d="M18 5v13"></path><path d="M22 10v3"></path></svg>',
    {
      headers: {
        'Content-Type': 'image/svg+xml',
      },
    },
  );
}
