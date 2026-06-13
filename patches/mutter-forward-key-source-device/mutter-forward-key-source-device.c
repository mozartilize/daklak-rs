/*
 * daklak local Mutter ForwardKeyEvent repair (LD_PRELOAD, inline-detour).
 *
 * Symbol interposition does NOT work for this bug, proven two ways:
 *
 *   1. clutter_input_method_forward_key() is reached from gnome-shell via
 *      GObject-Introspection, which resolves the C symbol with
 *      g_module_symbol() -> dlsym(module_handle, ...). dlsym on a dlopen'd
 *      handle searches that object's own scope, NOT the global LD_PRELOAD
 *      scope, so our exported override is never selected.
 *
 *   2. The internal forward_key -> clutter_event_key_new() call is locally
 *      bound: libmutter-clutter-18 is built -fno-semantic-interposition, so
 *      same-DSO calls to default-visibility globals bind to the local
 *      definition at link time. `readelf -rW` shows ZERO dynamic relocations
 *      naming clutter_event_key_new, confirming there is no interposable
 *      PLT/GOT slot for that call.
 *
 * So we make LD_PRELOAD work by NOT relying on symbol resolution. The
 * constructor finds the real clutter_input_method_forward_key() already mapped
 * in the host process and overwrites its prologue with a jump into our full
 * reimplementation. GI calls the real address; we own the bytes there.
 *
 * The reimplementation calls the REAL clutter_event_key_new() by resolved
 * address (not the locally-bound internal path) with a valid source device, so
 * the CLUTTER_IS_INPUT_DEVICE assertion passes and the synthetic key event is
 * created and delivered.
 *
 * This .so is loaded into EVERY process in the session (session-wide
 * LD_PRELOAD). It patches only processes where libmutter-clutter-18 is already
 * resident (RTLD_NOLOAD); every other process is left untouched.
 *
 * ABI-specific to /usr/lib/mutter-18 (mutter 50.x, GCC 16, x86-64 SysV).
 * Rebuild after a GNOME/Mutter upgrade.
 */

#define _GNU_SOURCE
#include <stdint.h>
#include <stddef.h>
#include <string.h>
#include <stdio.h>
#include <time.h>
#include <unistd.h>
#include <dlfcn.h>
#include <sys/mman.h>

/* ---- clutter constants (from clutter-enums.h, mutter 50.x) ---- */
#define CLUTTER_KEY_PRESS               1
#define CLUTTER_KEY_RELEASE             2
#define CLUTTER_EVENT_FLAG_INPUT_METHOD (1 << 1)

/* ClutterModifierType / ClutterEventType / gunichar are plain ints here. */
typedef struct {
  uint32_t pressed;
  uint32_t latched;
  uint32_t locked;
} ClutterModifierSet;

/* Exact ABI prototype of clutter_event_key_new(). raw_modifiers is a 12-byte
 * struct passed by value; letting the compiler see the real struct keeps the
 * SysV register/stack marshalling correct. */
typedef void *(*event_key_new_fn) (uint32_t            type,
                                    uint32_t            flags,
                                    int64_t             timestamp_us,
                                    void               *source_device,
                                    ClutterModifierSet  raw_modifiers,
                                    uint32_t            modifiers,
                                    uint32_t            keyval,
                                    uint32_t            evcode,
                                    uint32_t            keycode,
                                    uint32_t            unicode_value);

typedef void *(*get_backend_fn)        (void);
typedef void *(*get_default_seat_fn)   (void *backend);
typedef void *(*get_vsp_fn)            (void *seat);
typedef void  (*event_put_fn)          (void *event);
typedef void  (*event_free_fn)         (void *event);
typedef uint32_t (*keysym_to_unicode_fn)(uint32_t keyval);

static event_key_new_fn      real_event_key_new;
static get_backend_fn        p_get_backend;
static get_default_seat_fn   p_get_seat;
static get_vsp_fn            p_get_vsp;
static event_put_fn          p_event_put;
static event_free_fn         p_event_free;
static keysym_to_unicode_fn  p_keysym_to_unicode;

static void
daklak_log (const char *message)
{
  FILE *f = fopen ("/tmp/daklak-mutter-forward-key.log", "a");
  if (!f)
    return;
  fprintf (f, "%ld pid=%ld %s\n", (long) time (NULL), (long) getpid (), message);
  fclose (f);
}

/* Full replacement for clutter_input_method_forward_key(). Mirrors the
 * upstream body but supplies a valid source device and guards the NULL event. */
static void
daklak_forward_key (void     *im,
                    uint32_t  keyval,
                    uint32_t  keycode,
                    uint32_t  state,
                    uint64_t  time_,
                    int       press)
{
  void *backend, *seat, *source_device, *event;
  ClutterModifierSet raw = { 0, 0, 0 };

  (void) im;    /* upstream only g_return_if_fail's the type; trust the caller */
  (void) state; /* mirrors upstream: state is not propagated into the event */

  backend = p_get_backend ();
  if (!backend)
    return;

  seat = p_get_seat (backend);
  if (!seat)
    return;

  source_device = p_get_vsp (seat);
  if (!source_device)
    {
      daklak_log ("no virtual source pointer; dropping forward_key");
      return;
    }

  event = real_event_key_new (press ? CLUTTER_KEY_PRESS : CLUTTER_KEY_RELEASE,
                              CLUTTER_EVENT_FLAG_INPUT_METHOD,
                              (int64_t) time_,
                              source_device,
                              raw,
                              0,            /* modifiers */
                              keyval,
                              keycode - 8,  /* evdev_code */
                              keycode,      /* hardware_keycode */
                              p_keysym_to_unicode (keyval));
  if (!event)
    return;

  p_event_put (event);
  p_event_free (event);
}

/* Overwrite the first 16 bytes of `target` with:
 *   F3 0F 1E FA     endbr64           (preserve the CET/IBT landing pad)
 *   48 B8 <imm64>   movabs rax, replacement
 *   FF E0           jmp rax
 *
 * gnome-shell + libmutter-clutter-18 are built with IBT (NT_GNU_PROPERTY
 * x86 feature: IBT). GObject-Introspection invokes forward_key through an
 * INDIRECT call, so the patched entry MUST still begin with endbr64 or the
 * CPU raises #CP. The replacement handler is built -fcf-protection=full so it
 * has its own endbr64; the `jmp rax` into it is therefore also IBT-safe.
 */
#define DETOUR_LEN 16
static int
install_detour (void *target, void *replacement)
{
  long pagesize = sysconf (_SC_PAGESIZE);
  uintptr_t addr = (uintptr_t) target;
  uintptr_t page = addr & ~((uintptr_t) pagesize - 1);
  size_t span = (addr + DETOUR_LEN) - page;
  unsigned char patch[DETOUR_LEN];

  if (mprotect ((void *) page, span, PROT_READ | PROT_WRITE | PROT_EXEC) != 0)
    return -1;

  patch[0] = 0xF3;          /* endbr64 */
  patch[1] = 0x0F;
  patch[2] = 0x1E;
  patch[3] = 0xFA;
  patch[4] = 0x48;          /* REX.W */
  patch[5] = 0xB8;          /* mov rax, imm64 */
  memcpy (patch + 6, &replacement, 8);
  patch[14] = 0xFF;
  patch[15] = 0xE0;         /* jmp rax */
  memcpy (target, patch, DETOUR_LEN);

  mprotect ((void *) page, span, PROT_READ | PROT_EXEC);
  __builtin___clear_cache ((char *) target, (char *) target + DETOUR_LEN);
  return 0;
}

static void *
resolve (void *h, const char *name)
{
  void *sym = dlsym (h, name);
  if (!sym)
    {
      char buf[256];
      snprintf (buf, sizeof buf, "missing symbol: %s", name);
      daklak_log (buf);
    }
  return sym;
}

__attribute__((constructor))
static void
daklak_preload_init (void)
{
  void *h, *forward_key;

  /* Only act inside processes that already have the Clutter library mapped
   * (i.e. gnome-shell). RTLD_NOLOAD: never force it into other processes. */
  h = dlopen ("libmutter-clutter-18.so.0", RTLD_NOW | RTLD_NOLOAD | RTLD_GLOBAL);
  if (!h)
    return; /* not this process; stay invisible */

  forward_key        = resolve (h, "clutter_input_method_forward_key");
  real_event_key_new = (event_key_new_fn)     resolve (h, "clutter_event_key_new");
  p_get_backend      = (get_backend_fn)       resolve (h, "clutter_get_default_backend");
  p_get_seat         = (get_default_seat_fn)  resolve (h, "clutter_backend_get_default_seat");
  p_get_vsp          = (get_vsp_fn)           resolve (h, "clutter_seat_get_virtual_source_pointer");
  p_event_put        = (event_put_fn)         resolve (h, "clutter_event_put");
  p_event_free       = (event_free_fn)        resolve (h, "clutter_event_free");
  p_keysym_to_unicode= (keysym_to_unicode_fn) resolve (h, "clutter_keysym_to_unicode");

  if (!forward_key || !real_event_key_new || !p_get_backend || !p_get_seat ||
      !p_get_vsp || !p_event_put || !p_event_free || !p_keysym_to_unicode)
    {
      daklak_log ("symbol resolution incomplete; not patching");
      return;
    }

  if (install_detour (forward_key, (void *) daklak_forward_key) == 0)
    daklak_log ("installed inline detour on clutter_input_method_forward_key");
  else
    daklak_log ("mprotect failed; could not install detour");
}
