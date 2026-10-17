// CLSID_FileOpenDialog for skate.'s Wine prefix (install --native-picker). Wraps Wine's own dialog and forwards
// everything to it except Show: while the installer's picker-helper runs next to the game (picker.alive touched
// within 3 s), Show writes picker.request next to this DLL, the helper opens the desktop's file picker, and its
// answer comes back in picker.response. Without the helper, Wine's dialog opens as before.
#include <windows.h>
#include <shobjidl.h>
#include <shlobj.h>
#include <string>

static HMODULE self_module;

static std::wstring dir_file(const wchar_t* name) {
    wchar_t buf[MAX_PATH];
    GetModuleFileNameW(self_module, buf, MAX_PATH);
    std::wstring dir(buf);
    return dir.substr(0, dir.find_last_of(L'\\') + 1) + name;
}

static bool helper_alive() {
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExW(dir_file(L"picker.alive").c_str(), GetFileExInfoStandard, &data)) return false;
    FILETIME now;
    GetSystemTimeAsFileTime(&now);
    const auto t = [](FILETIME f) { return (ULONGLONG(f.dwHighDateTime) << 32) | f.dwLowDateTime; };
    return t(now) - t(data.ftLastWriteTime) < 3ull * 10000000;
}

static std::string utf8(const std::wstring& w) {
    std::string s(WideCharToMultiByte(CP_UTF8, 0, w.c_str(), -1, nullptr, 0, nullptr, nullptr), '\0');
    WideCharToMultiByte(CP_UTF8, 0, w.c_str(), -1, &s[0], int(s.size()), nullptr, nullptr);
    s.pop_back();
    return s;
}

static bool write_file(const std::wstring& path, const std::string& text) {
    const HANDLE h = CreateFileW(path.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS, 0, nullptr);
    if (h == INVALID_HANDLE_VALUE) return false;
    DWORD n;
    const bool ok = WriteFile(h, text.data(), DWORD(text.size()), &n, nullptr) && n == text.size();
    CloseHandle(h);
    return ok;
}

static bool read_file(const std::wstring& path, std::string& text) {
    const HANDLE h = CreateFileW(path.c_str(), GENERIC_READ, FILE_SHARE_READ, nullptr, OPEN_EXISTING, 0, nullptr);
    if (h == INVALID_HANDLE_VALUE) return false;
    char buf[4096];
    DWORD n;
    text.clear();
    while (ReadFile(h, buf, sizeof buf, &n, nullptr) && n) text.append(buf, n);
    CloseHandle(h);
    return true;
}

// The helper answers with a Linux path; Wine maps it to a drive (Z: for /).
static std::wstring dos_path(const std::string& unix_path) {
    using fn = WCHAR*(CDECL*)(LPCSTR);
    auto f = (fn)GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "wine_get_dos_file_name");
    WCHAR* w = f ? f(unix_path.c_str()) : nullptr;
    if (!w) return {};
    std::wstring r(w);
    HeapFree(GetProcessHeap(), 0, w);
    return r;
}

class Dialog : public IFileOpenDialog {
    LONG refs = 1;
    IFileOpenDialog* inner;
    FILEOPENDIALOGOPTIONS options = 0;
    std::wstring title, filter_name, filter_pattern;
    IShellItem* picked = nullptr;

    // 1: picked, 0: cancelled, -1: no helper (Wine's dialog opens instead).
    int ask_helper(HWND owner) {
        const auto request = dir_file(L"picker.request"), response = dir_file(L"picker.response");
        const std::string id = std::to_string(GetCurrentProcessId()) + "-" + std::to_string(GetTickCount64());
        DeleteFileW(response.c_str());
        const std::string text = id + "\n" + ((options & FOS_PICKFOLDERS) ? "folder" : "file") + "\n" + utf8(title) + "\n"
                                 + utf8(filter_name) + "\n" + utf8(filter_pattern) + "\n";
        // Renamed into place, so the helper never reads half of it.
        if (!write_file(dir_file(L"picker.request.tmp"), text)
            || !MoveFileExW(dir_file(L"picker.request.tmp").c_str(), request.c_str(), MOVEFILE_REPLACE_EXISTING)) return -1;
        if (owner) EnableWindow(owner, FALSE);
        int result = -1;
        for (;;) {
            MSG msg;
            while (PeekMessageW(&msg, nullptr, 0, 0, PM_REMOVE)) { TranslateMessage(&msg); DispatchMessageW(&msg); }
            std::string answer;
            if (read_file(response, answer)) {
                DeleteFileW(response.c_str());
                // id, then "ok" and the path, or "cancel".
                const size_t a = answer.find('\n'), b = answer.find('\n', a + 1), c = answer.find('\n', b + 1);
                if (a == std::string::npos || answer.substr(0, a) != id) continue;
                result = 0;
                if (b != std::string::npos && answer.substr(a + 1, b - a - 1) == "ok" && c != std::string::npos) {
                    const auto dos = dos_path(answer.substr(b + 1, c - b - 1));
                    if (!dos.empty() && SUCCEEDED(SHCreateItemFromParsingName(dos.c_str(), nullptr, IID_PPV_ARGS(&picked))))
                        result = 1;
                }
                break;
            }
            if (!helper_alive()) { DeleteFileW(request.c_str()); break; }
            MsgWaitForMultipleObjects(0, nullptr, FALSE, 100, QS_ALLINPUT);
        }
        if (owner) { EnableWindow(owner, TRUE); SetForegroundWindow(owner); }
        return result;
    }

public:
    explicit Dialog(IFileOpenDialog* inner) : inner(inner) {}
    virtual ~Dialog() { if (picked) picked->Release(); inner->Release(); }

    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** out) override {
        if (riid == IID_IUnknown || riid == IID_IModalWindow || riid == IID_IFileDialog || riid == IID_IFileOpenDialog) {
            *out = static_cast<IFileOpenDialog*>(this);
            AddRef();
            return S_OK;
        }
        // ponytail: other interfaces (IFileDialogCustomize, ...) come from Wine's dialog, and a Show through them skips
        // the helper. ReSkate only uses IFileOpenDialog; wrap more if it ever needs them.
        return inner->QueryInterface(riid, out);
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return InterlockedIncrement(&refs); }
    ULONG STDMETHODCALLTYPE Release() override {
        const ULONG n = InterlockedDecrement(&refs);
        if (!n) delete this;
        return n;
    }

    HRESULT STDMETHODCALLTYPE Show(HWND owner) override {
        if (picked) { picked->Release(); picked = nullptr; }
        if (helper_alive()) {
            const int r = ask_helper(owner);
            if (r == 1) return S_OK;
            if (r == 0) return HRESULT_FROM_WIN32(ERROR_CANCELLED);
        }
        return inner->Show(owner);
    }
    HRESULT STDMETHODCALLTYPE GetResult(IShellItem** item) override {
        if (picked) { *item = picked; picked->AddRef(); return S_OK; }
        return inner->GetResult(item);
    }
    HRESULT STDMETHODCALLTYPE GetResults(IShellItemArray** items) override {
        if (picked) return SHCreateShellItemArrayFromShellItem(picked, IID_PPV_ARGS(items));
        return inner->GetResults(items);
    }
    HRESULT STDMETHODCALLTYPE SetOptions(FILEOPENDIALOGOPTIONS o) override { options = o; return inner->SetOptions(o); }
    HRESULT STDMETHODCALLTYPE SetTitle(LPCWSTR t) override { title = t ? t : L""; return inner->SetTitle(t); }
    // ponytail: only the first filter goes to the desktop picker; ReSkate passes one.
    HRESULT STDMETHODCALLTYPE SetFileTypes(UINT n, const COMDLG_FILTERSPEC* f) override {
        if (n && f) { filter_name = f[0].pszName; filter_pattern = f[0].pszSpec; }
        return inner->SetFileTypes(n, f);
    }

    HRESULT STDMETHODCALLTYPE SetFileTypeIndex(UINT i) override { return inner->SetFileTypeIndex(i); }
    HRESULT STDMETHODCALLTYPE GetFileTypeIndex(UINT* i) override { return inner->GetFileTypeIndex(i); }
    HRESULT STDMETHODCALLTYPE Advise(IFileDialogEvents* e, DWORD* c) override { return inner->Advise(e, c); }
    HRESULT STDMETHODCALLTYPE Unadvise(DWORD c) override { return inner->Unadvise(c); }
    HRESULT STDMETHODCALLTYPE GetOptions(FILEOPENDIALOGOPTIONS* o) override { return inner->GetOptions(o); }
    HRESULT STDMETHODCALLTYPE SetDefaultFolder(IShellItem* s) override { return inner->SetDefaultFolder(s); }
    HRESULT STDMETHODCALLTYPE SetFolder(IShellItem* s) override { return inner->SetFolder(s); }
    HRESULT STDMETHODCALLTYPE GetFolder(IShellItem** s) override { return inner->GetFolder(s); }
    HRESULT STDMETHODCALLTYPE GetCurrentSelection(IShellItem** s) override { return inner->GetCurrentSelection(s); }
    HRESULT STDMETHODCALLTYPE SetFileName(LPCWSTR n) override { return inner->SetFileName(n); }
    HRESULT STDMETHODCALLTYPE GetFileName(LPWSTR* n) override { return inner->GetFileName(n); }
    HRESULT STDMETHODCALLTYPE SetOkButtonLabel(LPCWSTR t) override { return inner->SetOkButtonLabel(t); }
    HRESULT STDMETHODCALLTYPE SetFileNameLabel(LPCWSTR t) override { return inner->SetFileNameLabel(t); }
    HRESULT STDMETHODCALLTYPE AddPlace(IShellItem* s, FDAP p) override { return inner->AddPlace(s, p); }
    HRESULT STDMETHODCALLTYPE SetDefaultExtension(LPCWSTR e) override { return inner->SetDefaultExtension(e); }
    HRESULT STDMETHODCALLTYPE Close(HRESULT h) override { return inner->Close(h); }
    HRESULT STDMETHODCALLTYPE SetClientGuid(REFGUID g) override { return inner->SetClientGuid(g); }
    HRESULT STDMETHODCALLTYPE ClearClientData() override { return inner->ClearClientData(); }
    HRESULT STDMETHODCALLTYPE SetFilter(IShellItemFilter* f) override { return inner->SetFilter(f); }
    HRESULT STDMETHODCALLTYPE GetSelectedItems(IShellItemArray** s) override { return inner->GetSelectedItems(s); }
};

// Wine's dialog, straight from comdlg32: CoCreateInstance would land back here.
static HRESULT create_inner(IFileOpenDialog** out) {
    const HMODULE comdlg = LoadLibraryW(L"comdlg32.dll");
    auto get = comdlg ? (HRESULT(WINAPI*)(REFCLSID, REFIID, void**))GetProcAddress(comdlg, "DllGetClassObject") : nullptr;
    if (!get) return E_FAIL;
    IClassFactory* factory;
    HRESULT hr = get(CLSID_FileOpenDialog, IID_IClassFactory, (void**)&factory);
    if (FAILED(hr)) return hr;
    hr = factory->CreateInstance(nullptr, IID_IFileOpenDialog, (void**)out);
    factory->Release();
    return hr;
}

class Factory : public IClassFactory {
public:
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** out) override {
        if (riid == IID_IUnknown || riid == IID_IClassFactory) { *out = this; return S_OK; }
        *out = nullptr;
        return E_NOINTERFACE;
    }
    ULONG STDMETHODCALLTYPE AddRef() override { return 2; }
    ULONG STDMETHODCALLTYPE Release() override { return 1; }
    HRESULT STDMETHODCALLTYPE CreateInstance(IUnknown* outer, REFIID riid, void** out) override {
        *out = nullptr;
        if (outer) return CLASS_E_NOAGGREGATION;
        IFileOpenDialog* inner;
        HRESULT hr = create_inner(&inner);
        if (FAILED(hr)) return hr;
        auto dialog = new Dialog(inner);
        hr = dialog->QueryInterface(riid, out);
        dialog->Release();
        return hr;
    }
    HRESULT STDMETHODCALLTYPE LockServer(BOOL) override { return S_OK; }
};
static Factory factory;

extern "C" HRESULT WINAPI DllGetClassObject(REFCLSID clsid, REFIID riid, void** out) {
    if (clsid != CLSID_FileOpenDialog) return CLASS_E_CLASSNOTAVAILABLE;
    return factory.QueryInterface(riid, out);
}
extern "C" HRESULT WINAPI DllCanUnloadNow() { return S_FALSE; }

BOOL WINAPI DllMain(HINSTANCE module, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) { self_module = module; DisableThreadLibraryCalls(module); }
    return TRUE;
}
