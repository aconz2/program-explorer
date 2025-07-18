import { render, createRef, Component, RefObject } from 'preact';
import { signal, Signal } from '@preact/signals';
import { EditorState, Extension } from '@codemirror/state';
import { keymap, ViewUpdate } from '@codemirror/view';
import { EditorView, basicSetup } from 'codemirror';
import * as shlex from 'shlex';

import * as pearchive from './pearchive';
import {Api} from './api';
import {bufToHex, debounce, parseEnvText, bufToBase64} from './util';
import {UrlHashState, loadUrlHashState, encodeUrlHashState} from './urlstate';
import {parsePeToml} from './petoml';

import './style.css';

// busybox 1.36.0
const DEFAULT_IMAGE = "index.docker.io/library/busybox@sha256:086417a48026173aaadca4ce43a1e4b385e8e62cc738ba79fc6637049674cac0";
const DEFAULT_ARCH = "amd64";
const DEFAULT_OS = "linux";

const PETOML_NAME = "pe.toml";

const DEFAULT_INPUT_FILES = [
    {path:'test.sh', data:`
echo "hello world"
find /run/pe
cat /run/pe/input/folder/data.txt
echo "an error" 1>&2
echo "an output file" > /run/pe/output/output.txt
`.trimStart()
    },
    {path:'folder/data.txt', data:'hi this is some data'},
];

enum FileKind {
    Editor,
    Blob,
}

type FileId = string;
type ImageId = string;

type AppState = {
    selectedImage: Signal<ImageId | null>,
    selectedStdin: Signal<FileId | null>,
    selectedArch: Signal<FileId | null>,
    selectedOs: Signal<FileId | null>,
    cmd: Signal<string | null>,
    env: Signal<string[] | null>,
    lastStatus: Signal<string | null>,
    lastRuntime: Signal<number | null>
    running: Signal<boolean>,
}

const imageName = (info) => `${info.registry}/${info.repository}/${info.tag}`;

// TODO entrypoint
function computeFullCommand(env: string[] | null, userCmd: string)
    : {entrypoint: string[] | null, cmd: string[] | null, env: string[] | null} {
    let cmd = userCmd.length === 0 ? null : shlex.split(userCmd);
    let entrypoint = [];
    return {entrypoint, cmd, env};
}

type FileContents = string | ArrayBuffer;

class File {
    static _id = 0;

    id: string;
    path: string;
    kind: FileKind;
    editorState?: EditorState = null;
    data: FileContents;
    dataHex?: string = null;

    static _next_id(): number {
        File._id += 1;
        return File._id;
    }

    blobHex(len): string {
        if (this.kind !== FileKind.Blob) return '';
        if (this.dataHex === null) {
            // Uint8Array.toHex is only in ff right now
            //this.dataHex = new Uint8Array(this.data).slice(0, 100).toHex();
            if (!(this.data instanceof ArrayBuffer)) { throw new Error('type assertion'); }
            this.dataHex = bufToHex(this.data, len);
        }
        return this.dataHex;
    }

    length(): number {
        if (typeof this.data === 'string') return this.data.length;
        return this.data.byteLength;
    }

    constructor(path: string, kind: FileKind, data: FileContents, extensions: Extension[] = []) {
        this.id = File._next_id().toString();
        this.path = path;
        this.kind = kind;
        this.data = data;
        if (this.kind === FileKind.Editor) {
            if (typeof data !== 'string') throw new Error('type assertion');
            this.editorState = EditorState.create({
                doc: data,
                extensions: extensions,
            });
        }
    }

    static makeFile(path, data: FileContents, extensions?: Extension[]): File {
        if (typeof data === 'string') {
            return new File(path, FileKind.Editor, data, extensions);
        } else {
            return new File(path, FileKind.Blob, data, extensions);
        }
    }

    displayName() {
        return this.path;
    }
};

class FileStore {
    files: Map<FileId, File> = new Map();
    active?: string = null;
    extensions: Extension[];

    constructor(files?: Map<FileId, File>, active?: FileId, extensions?: Extension[]) {
        this.files = files ?? new Map();
        this.active = active ?? null;
        this.extensions = extensions ?? [];
    }

    from(inputs: {path: string, data: FileContents}[]): FileStore {
        if (inputs.length === 0) return new FileStore(null, null, this.extensions);
        let files = new Map(inputs.map(({path,data}) => {
            let f = File.makeFile(path, data, this.extensions);
            return [f.id, f];
        }));
        let active = files.keys().next().value;
        return new FileStore(files, active, this.extensions);
    }

    addTextFile(path: string, data: FileContents): FileStore {
        let f = File.makeFile(path, data, this.extensions);
        let files = this.files;
        files.set(f.id, f);
        let active = f.id;
        return new FileStore(files, active, this.extensions);
    }

    addFiles(inputs: {path: string, data: FileContents}[]): FileStore {
        if (inputs.length === 0) return this;
        let fs = inputs.map(({path,data}) => File.makeFile(path, data));
        let files = new Map(this.files);
        for (let f of fs) {
            files.set(f.id, f);
        }
        let active = this.active ?? files.keys().next().value;
        return new FileStore(files, active, this.extensions);
    }

    closeFile(file: File): FileStore {
        let files = this.files;
        files.delete(file.id);
        let active = (file.id === this.active) ? files.keys().next()?.value : this.active;
        return new FileStore(files, active, this.extensions);
    }

    newFile(): FileStore {
        let name = this._untitled_name();
        return this.addTextFile(name, '');
    }

    renameFile(id: FileId, name: string): FileStore {
        let file = this.files.get(id);
        if (file == null) {
            console.warn('renaming file not part of the store');
            return this;
        }
        let byName = new Set(Array.from(this.files.values(), x => x.path));
        if (byName.has(name)) {
            console.warn('renaming file to colliding name');
            return this;
        }
        file.path = name;
        return new FileStore(this.files, this.active, this.extensions);
    }

    setActive(file: File): FileStore {
        return new FileStore(this.files, file.id, this.extensions);
    }

    _untitled_name(): string {
        let byName = new Set(Array.from(this.files.values(), x => x.path));
        for (let i = 0; i < 100; i++) {
            let name = `Untitled-${i}`;
            if (!byName.has(name)) {
                return name;
            }
        }
        throw new Error('you got too many files');
    }

    getActive(): File | null {
        return this.files.get(this.active) ?? null;
    }
}

class SimpleEditor extends Component {
    editorParentRef = createRef();
    onChange: (a: string) => boolean;
    editor: EditorView;

    constructor({onChange=() => false}) {
        super();
        this.onChange = onChange;
    }

    componentDidMount() {
        let cb = this.onChange;
        this.editor = new EditorView({
          extensions: [
            basicSetup,
            EditorView.updateListener.of((v: ViewUpdate) => {
                if (v.docChanged) {
                    cb(v.state.doc.toString());
                }
            })
          ],
          parent: this.editorParentRef.current,
        });
    }

    toString(): string { return this.editor.state.doc.toString(); }

    replace(s: string) {
        console.log('replacing', this.editor.state.doc.length, s)
        this.editor.update([
            this.editor.state.update({changes: {from: 0, to: this.editor.state.doc.length, insert: s}})
        ]);
    }

    render() {
        return (
            <div class="editor-container">
                <div className="cm-container" ref={this.editorParentRef}></div>
            </div>
        );
    }
}

class Editor extends Component {
    editorParentRef = createRef();
    renameDialogRef = createRef();
    editor?: EditorView;
    readOnly: boolean;
    store: Signal<FileStore>;
    extensions: Extension[];
    renamingFileId?: FileId = null;

    constructor({readOnly=false, ctrlEnterCb=() => false}) {
        super();
        this.readOnly = readOnly;

        this.extensions = [
            EditorState.readOnly.of(readOnly),
            keymap.of([
                {key: 'Ctrl-Enter', run: ctrlEnterCb},
            ]),
            basicSetup,
        ];
        this.store = signal(new FileStore(null, null, this.extensions));
    }

    componentDidMount() {
        this.editor = new EditorView({
          extensions: [
              // we pass extensions through to the EditorState
          ],
          parent: this.editorParentRef.current,
        });
    }

    commitActive() {
        let store = this.store.value;
        let active = store.getActive();
        if (active?.editorState === null) return;
        active.editorState = this.editor.state;
        active.data = active.editorState.doc.toString();
    }

    getFiles(): File[] {
        this.commitActive();
        return Array.from(this.store.value.files.values());
    }

    addFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = this.store.value.addFiles(files);
        this.store.value = store;
    }

    setFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = this.store.value.from(files);
        let active = this.store.value.getActive();
        if (active !== null) {  // reselect active if path matches
            for (let f of store.files.values()) {
                if (f.path === active.path) {
                    store.active = f.id;
                    break;
                }
            }
        }
        let activeFile = store.getActive();
        // why can't you do activeFile?.editorState
        if (activeFile !== null && activeFile.editorState !== null) {
            this.editor.setState(activeFile.editorState);
        }
        this.store.value = store;
    }

    editFile(file: File) {
        this.commitActive();
        let store = this.store.value.setActive(file);
        if (file.editorState !== null) {
            this.editor.setState(file.editorState);
        }
        this.store.value = store;
    }

    newFile() {
        let store = this.store.value.newFile();
        let active = store.getActive();
        if (active?.editorState !== null) {
            this.editor.setState(active.editorState);
        }
        this.store.value = store;
    }

    closeFile(file: File) { // TODO undo
        let store = this.store.value.closeFile(file);
        let active = store.getActive();
        if (active?.editorState !== null) {
            this.editor.setState(active.editorState);
        }
        this.store.value = store;
    }

    renameFile(file: File) {
        if (this.readOnly) return;
        this.renamingFileId = file.id;
        this.showRenameDialog();
    }

    renameDialog() {
        return this.renameDialogRef.current;
    }

    showRenameDialog() {
        if (this.renameDialogRef.current == null) return;
        this.renameDialogRef.current.showModal();
    }

    onRenameSubmit(e) {
        // TODO when we submit with enter on the input, e.submitter is set to
        // whichever input/button is next in the form, not the one with the type="submit"
        const doRename = () => {
            let name = e.target.elements['name'].value;
            if (name === '') {
                console.warn('cant rename to empty string');
                return;
            }
            if (this.renamingFileId === null) {
                console.warn('cant rename without a fileid');
                return;
            }
            this.store.value = this.store.value.renameFile(this.renamingFileId, name);
        }
        switch (e.submitter?.value) {
            case 'rename': doRename(); break;
            case 'cancel': // fallthrough
            default:       // fallthrough
        }
        this.renamingFileId = null;
        e.target.elements['name'].value = '';
        this.renameDialog()?.close();
    }

    // this.props, this.state
    render() {
        let store = this.store.value;
        let tabs = Array.from(store.files.values(), (file: File) => {
            let className = 'tab-outer';
            if (store.active === file.id) {
                className += ' selected';
            }
            return (
                <span className={className}>
                    <button
                        className="tab-name"
                        key={file.id}
                        onClick={() => this.editFile(file)}
                        onDblClick={() => this.renameFile(file)}
                        title="Edit File"
                        >
                        {file.displayName()}
                    </button>
                    <button className="tab-close" title="Close File"
                        onClick={() => this.closeFile(file)}
                    ></button>
                </span>
            );
        });
        let newButton = this.readOnly ? [] : (
            <span className="tab-outer" title="New File">
                <button
                    className="tab-new"
                    onClick={() => this.newFile()}
                    >+</button>
            </span>
        );

        let active = store.getActive();

        let cmContainerStyle   = active?.editorState === null ? {display: 'none'} : {};
        let blobContainerStyle = active?.editorState !== null ? {display: 'none'} : {};
        const showBlobLength = 100;
        let blobContents = active?.kind === FileKind.Blob ? active.blobHex(showBlobLength) : '';
        let blobLength = active?.length();
        let blobLengthShown = Math.min(showBlobLength, blobLength ?? 0);

        return (
            <div class="editor-container">
                <div className="tab-row">
                    {tabs}
                    {newButton}
                </div>
                <div style={cmContainerStyle} className="cm-container" ref={this.editorParentRef}></div>
                <div style={blobContainerStyle} className="blob-container">
                    <p>Blob: showing {blobLengthShown}/{blobLength} bytes</p>
                    <pre>{blobContents}</pre>
                </div>
                <dialog ref={this.renameDialogRef}>
                    <form method="dialog" onSubmit={(e) => {e.preventDefault(); this.onRenameSubmit(e);}}>
                        <label for="name">Rename:</label>
                        <input autocomplete="off" name="name" type="text" />
                        <button value="rename" type="submit">Rename</button>
                        <button value="cancel">Cancel</button>
                        <p><kbd>Enter</kbd> to submit, <kbd>Esc</kbd> to cancel</p>
                    </form>
                </dialog>
            </div>
        );
    }
}

class App extends Component {
    r_inputEditor: RefObject<Editor> = createRef();
    r_outputEditor: RefObject<Editor> = createRef();
    r_envEditor: RefObject<SimpleEditor> = createRef();
    r_helpDetails: RefObject<HTMLDetailsElement> = createRef();
    r_moreDetails: RefObject<HTMLDetailsElement> = createRef();

    s: AppState = {
        selectedImage: signal(DEFAULT_IMAGE),
        selectedStdin: signal(null),
        selectedArch: signal(DEFAULT_ARCH),
        selectedOs: signal(DEFAULT_OS),
        cmd: signal(''),
        env: signal([]),
        lastStatus: signal(null),
        lastRuntime: signal(null),
        running: signal(false),
    };
    urlHashState: UrlHashState;

    constructor() {
        super();
        this.urlHashState = loadUrlHashState();
         console.log('loaded from url', this.urlHashState);
    }

    get inputEditor():  Editor { return this.r_inputEditor.current; }
    get outputEditor(): Editor { return this.r_outputEditor.current; }

    componentDidMount() {
        if (this.urlHashState.gist != null) {
            // TODO only supports id, not version
            this.loadFromGhGist(this.urlHashState.gist, null);
            return;
        }
        if (this.urlHashState.files != null) {
            console.log('loading files from url');
            this.inputEditor.setFiles(this.urlHashState.files);
            this.urlHashState.files = null; // clear mem
        } else {
            this.inputEditor.setFiles(DEFAULT_INPUT_FILES);
        }
        this.s.cmd.value = this.urlHashState.cmd ?? 'sh /run/pe/input/test.sh';
        this.s.selectedImage.value = this.urlHashState.image ?? DEFAULT_IMAGE;

        if (this.urlHashState.env != null) {
            this.r_envEditor.current.replace(this.urlHashState.env);
            this.onEnvChange(this.urlHashState.env);
        }
        if (this.urlHashState.stdin != null) {
            this.s.selectedStdin.value = this.urlHashState.stdin;
        }

        // for dev
        if (this.urlHashState.expand.help === true) {
            this.r_helpDetails.current.open = true;
        }
        if (this.urlHashState.expand.more === true) {
            this.r_moreDetails.current.open = true;
        }
    }

    async loadFromGhGist(id, version) {
        let req = new Request(Api.gh_gist(id, version), {
            method: 'GET',
        });
        const response = await fetch(req);
        if (!response.ok) {
            console.error(response);
            return;
        }
        let gist = await response.json();
        let files = [];
        for (let [path, data] of Object.entries(gist.files)) {
            if (path === PETOML_NAME) {
                let parsed = parsePeToml(data);
                if (parsed.env !== null) {
                    this.r_envEditor.current.replace(parsed.env);
                    this.onEnvChange(parsed.env);
                }
                if (parsed.stdin !== null) {
                    this.s.selectedStdin.value = parsed.stdin;
                }
                if (parsed.cmd !== null) {
                    this.s.cmd.value = parsed.cmd;
                }
                if (parsed.image !== null) {
                    this.s.selectedImage.value = parsed.image
                }
            } else {
                files.push({path, data});
            }
        }
        this.inputEditor.setFiles(files);
    }

    async run() {
        let selectedImage = this.s.selectedImage.value;
        let selectedStdin = this.s.selectedStdin.value;
        let selectedArch = this.s.selectedArch.value;
        let selectedOs = this.s.selectedOs.value;
        let cmd = this.s.cmd.value;
        let env = this.s.env.value;

        if (cmd === null) {
            console.warn('cant run without a cmd');
            return;
        }

        let fullCommand;
        try {
            fullCommand = computeFullCommand(env, cmd);
        } catch (e) {
            console.warn('cmd bad split');
            console.error(e);
            return;
        }

        let archive = pearchive.packArchiveV1(this.inputEditor.getFiles());
        let runReq = {
            stdin: selectedStdin,
            ...fullCommand,
        };
        console.log(runReq);
        let combined = pearchive.combineRequestAndArchive(runReq, archive);

        let req = new Request(Api.apiv2_runi(selectedImage, selectedArch, selectedOs), {
            method: 'POST',
            body: combined,
            headers: {
                'Content-type': 'application/x.pe.archivev1',
            }
        });
        const response = await fetch(req);
        // TODO handle 429
        if (!response.ok) {
            console.error(response);
            return;
        }
        const body = await response.arrayBuffer();
        let [responseJson, archiveSlice] = pearchive.splitResponseAndArchive(body);
        console.log(responseJson);
        let responseTyped: Api.Runi.Response = responseJson;
        let lastStatus = (() => {
            switch (responseTyped.kind) {
                case 'Ok': return {Ok: {siginfo: responseTyped.siginfo}};
                case 'Overtime': return {Overtime: {siginfo: responseTyped.siginfo}};
                case 'Panic': return {Panic: {message: responseTyped.message}};
            }
            return null;
        })();
        this.s.lastStatus.value = lastStatus != null ? JSON.stringify(lastStatus) : null;

        let returnFiles = pearchive.unpackArchiveV1(archiveSlice);
        returnFiles.sort((a, b) => {
            if (a.path === 'stdout' && b.path === 'stderr') return -1;
            if (a.path === 'stderr' && b.path === 'stdout') return 1;
            if (a.path === 'stdout' || a.path == 'stderr') return -1;
            if (b.path === 'stdout' || b.path == 'stderr') return 1;

            a.path.localeCompare(b.path)
        });
        //console.log(returnFiles);
        //console.log(archiveSlice);
        this.outputEditor.setFiles(returnFiles);
    }

    async onRun() {
        this.s.running.value = true;
        this.s.lastRuntime.value = null;
        let start = performance.now();
        try {
            await this.run();
        } catch (error) {
            console.error(error);
        }
        this.s.lastRuntime.value = performance.now() - start;
        this.s.running.value = false;
    }

    onImageSelect(event) {
        this.s.selectedImage.value = event.target.value;
    }

    onStdinSelect(event) {
        this.s.selectedStdin.value = event.target.value;
    }

    onOsSelect(event) {
        this.s.selectedOs.value = event.target.value;
    }

    onArchSelect(event) {
        this.s.selectedArch.value = event.target.value;
    }

    onCmdChange(event) {
        this.s.cmd.value = event.target.value;
    }

    onEnvChange(env: string) {
        if (env.length === 0) {
            this.s.env.value = null;
        } else {
            this.s.env.value = parseEnvText(env);
        }
    }

    onSaveToUrl(event) {
        event.preventDefault();
        let saveState = {
            cmd: this.s.cmd.value,
            stdin: this.s.selectedStdin.value,
            // doc is technically always there but .state can return an empty object
            env: this.r_envEditor.current.toString(),
            image: this.s.selectedImage.value,
            files: this.inputEditor?.getFiles().map(file => {
                return (typeof file.data === 'string') ?
                {p: file.path, s: file.data} : { p: file.path, b: bufToBase64(file.data)};
            }),
        };
        console.log('saving state', saveState);
        let s = '';
        if (this.urlHashState.expand.more === true) { s += 'more=x&'; }
        s += 's=' + encodeUrlHashState(saveState);
        window.location.hash = s;
    }

    onClearUrl(event) {
        event.preventDefault();
        if (this.urlHashState.expand.more === true) {
            window.location.hash = "more=x";
        } else {
            window.location.hash = "";
        }
    }

    render() {
        let selectedImage = this.s.selectedImage.value;
        let cmd = this.s.cmd.value;
        let lastStatus = this.s.lastStatus.value;
        let lastRuntime = this.s.lastRuntime.value;
        let env = this.s.env.value;

        let stdinOptions = this.inputEditor?.getFiles().map(file => {
            return <option key={file.id} value={file.path}>{file.path}</option>;
        });

        let imageDetails = null;
        let fullCommand = null;
        try {
            fullCommand = computeFullCommand(env, cmd);
        } catch (e) {
            console.error(e);
        }
        //if (selectedImage !== null) {
        //    let image = images.get(selectedImage);
        //    imageDetails = (
        //        <details>
        //            <summary>About this image</summary>
        //            <a target="_blank" href={image.links.upstream} rel="nofollow">{imageName(image.info)}</a>
        //            <p>These are a subset of properties defined for this image. See <a href="https://github.com/opencontainers/image-spec/blob/main/config.md#properties" rel="nofollow">the OCI image spec</a> for more information.</p>
        //            <dl>
        //                <dt>Env</dt>
        //                <dd class="mono">{JSON.stringify(image.config.config.Env ?? [])}</dd>
        //                <dt>Entrypoint</dt>
        //                <dd class="mono">{JSON.stringify(image.config.config.Entrypoint ?? [])}</dd>
        //                <dt>Cmd</dt>
        //                <dd class="mono">{JSON.stringify(image.config.config.Cmd ?? [])}</dd>
        //                <dd class="mono">
        //
        //                </dd>
        //            </dl>
        //            <details>
        //                <summary>Full <a href="https://github.com/opencontainers/image-spec/blob/main/config.md" rel="nofollow">OCI Image Config</a></summary>
        //                <pre>{JSON.stringify(image.config, null, '  ')}</pre>
        //            </details>
        //        </details>
        //    );
        //    try {
        //        fullCommand = computeFullCommand(image, env, cmd);
        //    } catch (e) {
        //        fullCommand = null;
        //    }
        //}
        return (
            <div className="mono">
                <div>
                    <button onClick={e => this.onSaveToUrl(e)}>Save To URL</button>
                    <button onClick={e => this.onClearUrl(e)}>Clear URL</button>
                </div>

                <hr />

                <details ref={this.r_helpDetails}>
                    <summary>Help</summary>
                    <p>Input size limited to 1 MB</p>
                    <p>Runtime limited to 1 second</p>
                    <p>Input files are in <code>/run/pe/input</code></p>
                    <p>Output files go in <code>/run/pe/output</code> (they will get prefixed with a <code>dir/</code>)</p>
                    <p>Double-click a filename to rename it</p>
                    <p>A default <code class="inline">PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin</code> is implicitly added on the backend if one isn't set by you or the image config as is done by docker/podman/kata</p>
                    <p><code>stdout</code> and <code>stderr</code> are captured</p>
                    <p><code>stdin</code> can be attached to an input file (under More)</p>
                    <p><kbd>Ctrl+Enter</kbd> within text editor will run</p>
                    <p>Images can be from docker.io and ghcr.io</p>
                    <p>Images can be a maximum of 2GB total layer size (compressed) and 3GB when uncompressed</p>
                </details>

                <hr />

                <form>
                    <div>
                        <label class="inline-label" for="image">Image</label>
                        <input type="text" value={this.s.selectedImage} name="image" onChange={e => this.onImageSelect(e)} />
                    </div>
                    {imageDetails}
                    <hr />
                    <div>
                        <label class="inline-label" for="cmd">Command</label>
                        <input autocomplete="off" name="cmd" className="mono" type="text"
                               value={cmd} onInput={e => this.onCmdChange(e)} />
                    </div>
                    <details ref={this.r_moreDetails}>
                        <summary>More</summary>
                        <label for="arch">Architecture</label>
                        <select value={this.s.selectedArch} name="arch" onChange={e => this.onArchSelect(e)}>
                            <option value="amd64">amd64</option>
                        </select>

                        <br />

                        <label for="os">OS</label>
                        <select value={this.s.selectedOs} name="os" onChange={e => this.onOsSelect(e)}>
                            <option value="linux">linux</option>
                        </select>

                        <br />

                        <label for="stdin">stdin</label>
                        <select value={this.s.selectedStdin} name="stdin" onChange={e => this.onStdinSelect(e)}>
                            <option value="/dev/null">/dev/null</option>
                            {stdinOptions}
                        </select>

                        <div id="env-editor">
                            <label for="env">env</label>
                            <SimpleEditor
                                 onChange={debounce(this.onEnvChange.bind(this), 250)}
                                 ref={this.r_envEditor} />
                        </div>

                        <h3>Computed</h3>
                        {fullCommand === null ? (<div>error in cmd</div>) : (
                            <dl>
                                <dt>Env</dt>
                                <dd>{JSON.stringify(fullCommand.env)}</dd>
                                <dt>Entrypoint</dt>
                                <dd>{JSON.stringify(fullCommand.entrypoint)}</dd>
                                <dt>Cmd</dt>
                                <dd>{JSON.stringify(fullCommand.cmd)}</dd>
                            </dl>
                        )}
                    </details>
                    <div>
                        <button
                            className="mono"
                            onClick={e => {
                                e.preventDefault();
                                if (this.s.running.value) return;
                                this.onRun();
                            }}
                            disabled={this.s.running}
                            >
                            {this.s.running.value ? 'Running…' : 'Run'}
                        </button>
                    </div>
                </form>

                <div id="input-output-container">
                    <div id="input-container">
                        <Editor
                            ref={this.r_inputEditor}
                            ctrlEnterCb={(_editorView) => { this.onRun(); return true; }}
                            />
                    </div>
                    <div id="output-container">
                        <span className="mono">{lastRuntime === null ? '' : `${lastRuntime.toFixed(2)}ms`}</span>
                        <span className="mono">{lastStatus ?? ''}</span>
                        <Editor
                            ref={this.r_outputEditor}
                            readOnly={true} />
                    </div>
                </div>
        </div>
        );
    }
}

render(<App/>, document.getElementById('app'));
