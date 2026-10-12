# Opening photos from macOS

Finder's Open With, opening a file whose chosen default is LightCraft, and dropping files on the
Dock icon add those files to the current library in place. The original stays where it is.
LightCraft selects the imported photo and shows Detail. Opening an existing photo selects it
without creating a duplicate. The normal import rules apply to sidecars, raw defaults,
Recently Deleted, unreadable files and unsupported formats.

Opening a document uses the same add mode as dropping a photo on the window or passing a file on
the command line. Use File > Import Photos to choose Copy or Move. This is an import into My Photos;
Local > Browse Folder remains the way to browse without adding photos to My Photos (LR-IMP-LOCAL).

Requests received while starting, importing, scanning, synchronizing or showing an Import Photos
dialog (including its file picker) wait in order. Each request is bound to the current library
opening when received. Open Library, Open Recent Library and New Library refuse to switch while
requests or imports are outstanding and explain why. Each running import also checks its library
identity before starting its worker or committing its results.

Startup recovery is the exception: a failed library must be reopened, replaced, or explicitly
continued without saving before requests acquire their destination and the queue drains. This
choice remains available during recovery. The queue lasts for this process only; quitting discards
requests that have not started.

If Import Photos is confirmed while another import is active, confirmation is rejected with a
message. The dialog keeps the chosen files and options. Wait for the active import to finish,
then click Import again. A second confirmation never replaces or cancels the active worker.

## Bundle registration

`packaging/macos/package.sh`, called by `.github/workflows/release.yml`, assembles and signs the
app. It substitutes version fields in `Info.plist.in`, then runs `document_types.py` before signing.
The generator reads `lightcraft_engine::import::EXTENSIONS`, registers each supported extension
and references macOS's existing Uniform Type Identifiers (UTIs). CoreTypes and RawCameraTypes
declarations available on the macOS 11 deployment baseline are not redeclared. Only JPEG XL,
AVIF, X3F and KDC receive imported declarations with filename tags for compatibility on supported
systems that lack them. Every document type has role Editor and rank Alternate.

The app does not explicitly set a default handler. LaunchServices can nevertheless choose an
Alternate handler as the default when no higher-ranked handler exists for that type. This is
acceptable for LightCraft; Alternate does not guarantee that the app can never become a default.

When changing EXTENSIONS, refresh the template for review and the plist-sync test:

```sh
python3 packaging/macos/document_types.py packaging/macos/Info.plist.in
cargo test -p lightcraft macos_document_types
```

The test compares the template with the compiled extension list and checks ranks, roles, system
UTI references and the exact compatibility declarations and their extension tags. HEIC/HEIF registration does not enable their optional decoder; builds
without it report the normal import error. AVIF likewise retains the existing decoder limitation.

## Event handling

The pinned eframe 0.36 / winit 0.30 stack does not forward document opens. `lightcraft-macos-open`
registers the Open Documents (`aevt/odoc`) Apple event after creating NSApplication and before
starting eframe. A notification observer registers it again at `NSApplicationWillFinishLaunching`,
after AppKit installs its handlers and before cold-start documents arrive. This is the event AppKit
normally translates to `application:openURLs:` or `application:openFiles:`. The bridge leaves
winit's application delegate and quit handling intact.
It holds its Objective-C handler until shutdown and removes its registration when dropped.

Foundation converts file descriptors to file URLs, including legacy alias descriptors. Rust keeps
owned paths. The event callback does not probe or decode photos. The desktop queue starts the
normal background import when the library is ready. Non-UTF-8 paths are reported instead of
silently changing the filename; Unicode, spaces and percent signs are preserved. Missing files
and unavailable volumes follow the importer's error handling.

References: [Apple event manager](https://developer.apple.com/documentation/foundation/nsappleeventmanager),
[AppKit document URLs](https://developer.apple.com/documentation/appkit/nsapplicationdelegate/application(_:open:)),
[system-declared UTIs](https://developer.apple.com/library/archive/documentation/Miscellaneous/Reference/UTIRef/Articles/System-DeclaredUniformTypeIdentifiers.html),
[UTI declarations](https://developer.apple.com/library/archive/documentation/FileManagement/Conceptual/understanding_utis/understand_utis_declare/understand_utis_declare.html),
[handler rank](https://developer.apple.com/documentation/bundleresources/information-property-list/cfbundledocumenttypes/lshandlerrank).

## Native verification

Use an isolated configuration and library, then open a raw through LaunchServices:

```sh
open -n -a /path/to/LightCraft.app \
  --env LIGHTCRAFT_CONFIG_DIR=/path/to/scratch/config \
  /path/to/photo.ARW --args --library /path/to/scratch/library --no-demo --control 18741
open -a /path/to/LightCraft.app /path/to/another-photo.dng
```

Use the control channel to check selection, the displayed photo, duplicate count and import errors.
Test startup with a blocked library as well as a running app, multiple files, an alias, Unicode
names and a missing file. Do not set system handlers. Unregister a temporary test bundle afterwards:

```sh
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -u /path/to/LightCraft.app
```

### Round 1 verification on macOS 26.6.2, Apple Silicon

A local ad-hoc signed bundle was tested with a separate bundle identifier, configuration folder,
and library. `open -a` delivered a cold Sony ARW before the library opened, then a warm panorama
DNG. Both rendered in Detail. The DNG screenshot came from the native Metal window, with no
headless screenshot fallback. LaunchServices listed the bundle for ARW, DNG, JPEG, HEIF, X3F
and KDC; no default handlers were set.

A duplicate ARW opened from Picks kept the catalog at two photos and selected the original.
A request containing a Unicode PNG filename with spaces, `#` and `%`, plus an uppercase JPG,
added both. Opening a Finder alias selected its original without adding another photo. A text file
and an explicit Apple event for a missing ARW produced import errors and left the app responsive.
With a file blocking the requested library folder, a cold DNG and a subsequent warm ARW stayed
queued. Opening a new library drained both requests and selected the ARW.

These checks used the same Open Documents event as Finder and Dock. A physical Finder double-click,
a physical Dock drop, an actual network share and Intel macOS were not tested. The test bundle was
unregistered after the checks. The temporary #990 and #997 build fixes were used only for testing.

### Round 2 verification on macOS 26.6.2, Apple M3 Max

Direct Open Documents events targeted an isolated test process by PID. With Import Photos open,
the native request stayed queued and the catalog remained empty. Switching libraries was refused
with a visible message. Confirming the dialog imported both the manual and native photos into A,
selected the native photo, and rendered Detail. Reopening it kept the count at two. Switching to
B after completion showed an empty library, and reopening A restored both photos.

With a file blocking the startup library folder, a native request waited until another library
was chosen, then imported there. Native Metal screenshots of the blocked switch and Detail were
inspected; neither used the headless fallback. The test process used a separate configuration and
libraries. No bundle was registered, and no defaults were changed.

The regression suite also checks a blocked native worker during manual confirmation, native file
pickers, stale import results after a host replaces its library, all recovery choices, and reopening
the recovery window after choosing Continue Without Saving. Physical Finder/Dock gestures,
network shares, Intel macOS and macOS 11 were not retested in this round.
