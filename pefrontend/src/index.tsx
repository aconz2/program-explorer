import { render, createRef, Component, RefObject } from 'preact';
import { signal, Signal } from '@preact/signals';
import { EditorState, Extension } from '@codemirror/state';
import { keymap } from '@codemirror/view';
import { EditorView, basicSetup } from 'codemirror';
import * as shlex from 'shlex';

import * as pearchive from './pearchive';
import {Api} from './api';
import {bufToHex, debounce} from './util';
import {UrlHashState, loadUrlHashState, encodeUrlHashState} from './urlstate';

import './style.css';

enum FileKind {
    Editor,
    Blob,
}

type FileId = string;
type ImageId = string;

type AppState = {
    images: Signal<Map<ImageId, Api.Image>>,
    selectedImage: Signal<ImageId | null>,
    selectedStdin: Signal<FileId | null>,
    cmd: Signal<string | null>,
    lastStatus: Signal<string | null>,
    lastRuntime: Signal<number | null>
    running: Signal<boolean>,
}

const imageName = (info) => `${info.registry}/${info.repository}/${info.tag}`;

function computeFullCommand(image: Api.Image, env: [string] | null, userCmd: string)
    : {entrypoint: string[], cmd: string[], env: string[]} {
    let parts = userCmd.length === 0 ? (image.config.config.Cmd ?? []) : shlex.split(userCmd);
    // let entrypoint = image.config.config.Entrypoint ?? [];
    let entrypoint = [];
    env = env ?? image.config.config.Env ?? [];
    return {entrypoint, cmd: parts, env};
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

    blobHex(): string {
        if (this.kind !== FileKind.Blob) return '';
        if (this.dataHex === null) {
            // Uint8Array.toHex is only in ff right now
            //this.dataHex = new Uint8Array(this.data).slice(0, 100).toHex();
            if (!(this.data instanceof ArrayBuffer)) { throw new Error('type assertion'); }
            this.dataHex = bufToHex(this.data);
        }
        return this.dataHex;
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
        let blobContents = active?.kind === FileKind.Blob ? active.blobHex() : '';

        return (
            <div class="editor-container">
                <div className="tab-row">
                    {tabs}
                    {newButton}
                </div>
                <div style={cmContainerStyle} className="cm-container" ref={this.editorParentRef}></div>
                <div style={blobContainerStyle} className="blob-container">
                    <p>Blob: first 100 bytes</p>
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
    r_envEditor: RefObject<Editor> = createRef();
    r_helpDetails: RefObject<HTMLDetailsElement> = createRef();
    r_moreDetails: RefObject<HTMLDetailsElement> = createRef();

    s: AppState = {
        images: signal(new Map()),
        selectedImage: signal(null),
        selectedStdin: signal(null),
        cmd: signal(null),
        env: signal(null),
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
        if (this.urlHashState.files != null) {
            this.inputEditor.setFiles(this.urlHashState.files);
            this.urlHashState.files = null; // clear mem
        } else {
            this.inputEditor.setFiles([
                {path:'test.sh', data:'echo "hello world"\ncat /run/pe/input/f1/dataf1.txt > /run/pe/output/data.txt\nls -ln /run/pe\ncat /run/pe/input/blob > /run/pe/output/blob\necho "an error" 1>&2'},
                {path:'blob', data: new Uint8Array([254, 237, 186, 202, 0, 10, 0]).buffer},
                //{path:'data.txt', data:'hi this is some data'},
                {path:'f1/dataf1.txt', data:'hi this is some data1'},
                {path:'f1/f2/dataf1f2.txt', data:'hi this is some data2'},
                {path:'f2/dataf2.txt', data:'hi this is some datar3'},
            ]);
        }
        this.s.cmd.value = this.urlHashState.cmd ?? 'sh /run/pe/input/test.sh';

        this.fetchImages();

        if (this.urlHashState.expand.help === true) {
            this.r_helpDetails.current.open = true;
        }
        if (this.urlHashState.expand.more === true) {
            this.r_moreDetails.current.open = true;
        }

        //setTimeout(() => {
        //    let y = pearchive.packArchiveV1(this.inputEditor.getFiles());
        //    // only firefox has a Blob.bytes() method
        //
        //    y.arrayBuffer().then(buf=>{
        //        let bytes = new Uint8Array(buf);
        //        console.log('----------------   packed -----------------------');
        //        console.log(bytes);
        //        console.log('---------------- unpacked (uint8array) -----------------------');
        //        console.log(pearchive.unpackArchiveV1(bytes));
        //        console.log('---------------- unpacked2 (arraybuffer) -----------------------');
        //        console.log(pearchive.unpackArchiveV1(buf));
        //        //console.log('---------------- unpacked2 (dataview) -----------------------');
        //        //console.log(pearchive.unpackArchiveV1(new DataView(buf, 62)));
        //    });
        //}, 100);
    }

    async run() {
        //let {images,selectedImage,cmd} = this.state;
        let selectedImage = this.s.selectedImage.value;
        let selectedStdin = this.s.selectedStdin.value;
        let cmd = this.s.cmd.value;
        let env = this.s.env.value;

        if (selectedImage === null) {
            console.warn('cant run without an image');
            return;
        }
        if (cmd === null) {
            console.warn('cant run without a cmd');
            return;
        }

        let image = this.s.images.value.get(selectedImage);
        if (image === undefined) {
            console.warn('cant run without an image');
            return;
        }

        let fullCommand;
        try {
            fullCommand = computeFullCommand(image, env, cmd);
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

        let req = new Request(window.location.origin + image.links.runi, {
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

    async fetchImages() {
        let response = await fetch(window.location.origin + '/api/v1/images');
        if (response.ok) {
            let json: Api.Images.Response = await response.json();
            if (json.images?.length > 0) {
                let images: Map<string, Api.Image> = new Map(json.images.map(image => [image.info.digest, image]));
                this.s.images.value = images;
                // check from urlhashstate and verify it is in the set, otherwise error
                this.s.selectedImage.value = images.keys().next()?.value;
            }
        } else {
            console.error(response);
        }
    }

    onImageSelect(event) {
        this.s.selectedImage.value = event.target.value;
    }

    onStdinSelect(event) {
        this.s.selectedStdin.value = event.target.value;
    }

    onCmdChange(event) {
        this.s.cmd.value = event.target.value;
    }

    onEnvChange(env: string) {
        if (env.length === 0) {
            this.s.env.value = null;
        } else {
            console.log('parsingenvtext')
            this.s.env.value = parseEnvText(env);
        }
    }

    onSaveToUrl(event) {
        event.preventDefault();
        let s = '';
        if (this.urlHashState.expand.more === true) { s += 'more=x&'; }
        //for (let file of this.inputEditor?.getFiles()) {
        //    if (typeof file.data !== 'string') {
        //        console.error('binary files not supported yet...');
        //        return;
        //    }
        //}
        s += 's=' + encodeUrlHashState({
            cmd: this.s.cmd.value,
            stdin: this.s.selectedStdin.value,
            env: this.s.env.value,
            image: this.s.selectedImage.value,
            files: this.inputEditor?.getFiles().map(file => {
                return {
                    p: file.path,
                    s: file.data,
                };
            }),
        });
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
        let images = this.s.images.value;
        let selectedImage = this.s.selectedImage.value;
        let cmd = this.s.cmd.value;
        let lastStatus = this.s.lastStatus.value;
        let lastRuntime = this.s.lastRuntime.value;
        let env = this.s.env.value;

        // firefox supports Map().values().map(), but chrome doesn't
        let imageOptions = Array.from(images.values(), ({info,links}) => {
            let name = imageName(info);
            return <option key={info.digest} value={info.digest}>{name}</option>;
        });

        let stdinOptions = this.inputEditor?.getFiles().map(file => {
            return <option key={file.id} value={file.path}>{file.path}</option>;
        });

        let imageDetails = null;
        let fullCommand = null;
        if (selectedImage !== null) {
            let image = images.get(selectedImage);
            imageDetails = (
                <details>
                    <summary>About this image</summary>
                    <a target="_blank" href={image.links.upstream} rel="nofollow">{imageName(image.info)}</a>
                    <p>These are a subset of properties defined for this image. See <a href="https://github.com/opencontainers/image-spec/blob/main/config.md#properties" rel="nofollow">the OCI image spec</a> for more information.</p>
                    <dl>
                        <dt>Env</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Env ?? [])}</dd>
                        <dt>Entrypoint</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Entrypoint ?? [])}</dd>
                        <dt>Cmd</dt>
                        <dd class="mono">{JSON.stringify(image.config.config.Cmd ?? [])}</dd>
                    </dl>

                </details>
            );
            try {
                fullCommand = computeFullCommand(image, env, cmd);
            } catch (e) {
                fullCommand = null;
            }
        }
        return (
            <div className="mono">
                <details ref={this.r_helpDetails}>
                    <summary>Help</summary>
                    <p>Input size limited to 1 MB</p>
                    <p>Runtime limited to 1 second</p>
                    <p>Input files are in <code>/run/pe/input</code></p>
                    <p>Output files go in <code>/run/pe/output</code></p>
                    <p><code>stdout</code> and <code>stderr</code> are captured</p>
                    <p><code>stdin</code> can be attached to an input file (under Advanced)</p>
                    <p><kbd>Ctrl+Enter</kbd> within text editor will run</p>
                </details>

                <form>
                    <div>
                        <label for="image">Image</label>
                        <select name="image" onChange={e => this.onImageSelect(e)}>
                            {imageOptions}
                        </select>
                    </div>
                    {imageDetails}
                    <div>
                        <input autocomplete="off" id="cmd" className="mono" type="text"
                               value={cmd} onInput={e => this.onCmdChange(e)} />
                    </div>
                    <details ref={this.r_moreDetails}>
                        <summary>More</summary>
                        <label for="stdin">stdin</label>
                        <select name="stdin" onChange={e => this.onStdinSelect(e)}>
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
                        <div>
                            <button onClick={e => this.onSaveToUrl(e)}>Save To URL</button>
                            <button onClick={e => this.onClearUrl(e)}>Clear URL</button>
                        </div>
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
