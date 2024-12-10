import { render, createRef, Component, RefObject } from 'preact';
import { signal, Signal } from '@preact/signals';
import { EditorState, Extension } from '@codemirror/state';
import { keymap } from '@codemirror/view';
import { EditorView, basicSetup } from 'codemirror';

import * as pearchive from './pearchive';
import {Api} from './api';

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
    cmd: Signal<string | null>,
    lastStatus: Signal<string | null>,
}

function bufToHex(data: ArrayBuffer, length=100): string {
    let n = Math.min(data.byteLength, length);
    let acc = '';
    let hexDigit = (i) => '0123456789abcdef'[i];
    if (data instanceof ArrayBuffer) {
        let buf = new Uint8Array(data);
        for (let i = 0; i < n; i++) {
            let b = buf[i];
            acc +=  hexDigit((b >> 4) & 0xf) + hexDigit(b & 0xf);
        }
        return acc;
    }
    throw new Error('bad type');
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

const imageName = (info) => `${info.registry}/${info.repository}/${info.tag}`;

// TODO this just unconditionally overrides Entrypoint and Cmd with a string split of cmd
function computeFullCommand(image: Api.Image, userCmd: string): {entrypoint?: string[], cmd?: string[]} {
    // let env = image.config.config.Env ?? [];
    let parts = userCmd.split(/\s+/); // TODO handle quotes
    // todo handle env
    //if (parts[0] === '$entrypoint') {
    //    acc.extend(image.config.config.Entrypoint ?? []);
    //    parts = parts.slice(1);
    //}
    //if (parts[0] === '$cmd') {
    //    acc.extend(image.config.config.Cmd ?? []);
    //    parts = parts.slice(1);
    //}
    return {entrypoint: [], cmd: parts};
}

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
        if (activeFile?.editorState !== null) {
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

    componentDidMount() {
        this.editor = new EditorView({
          extensions: [
              // we pass extensions through to the EditorState
          ],
          parent: this.editorParentRef.current,
        });

        //if (!this.readOnly) {
        //setTimeout(() => { this.showRenameDialog(); }, 100);
        //}
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
            <div class="editorContainer">
                <div className="tab-row">
                    {tabs}
                    {newButton}
                </div>
                <div style={cmContainerStyle} className="cmContainer" ref={this.editorParentRef}></div>
                <div style={blobContainerStyle} className="blobContainer">
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
    // would like to call this state but not sure if that messes with the Component state
    s: AppState = {
        images: signal(new Map()),
        selectedImage: signal(null),
        cmd: signal(null),
        lastStatus: signal(null),
    };

    get inputEditor():  Editor { return this.r_inputEditor.current; }
    get outputEditor(): Editor { return this.r_outputEditor.current; }

    componentDidMount() {
        // if you execute these back to back they don't both get applied...
        this.inputEditor.setFiles([
            {path:'test.sh', data:'echo "hello world"\ncat /run/pe/input/f1/dataf1.txt > /run/pe/output/data.txt\nls -ln /run/pe\ncat /run/pe/input/blob > /run/pe/output/blob\necho "an error" 1>&2'},
            {path:'blob', data: new Uint8Array([254, 237, 186, 202]).buffer},
            //{path:'data.txt', data:'hi this is some data'},
            {path:'f1/dataf1.txt', data:'hi this is some data1'},
            {path:'f1/f2/dataf1f2.txt', data:'hi this is some data2'},
            {path:'f2/dataf2.txt', data:'hi this is some datar3'},
        ]);
        this.s.cmd.value = 'sh /run/pe/input/test.sh';

        this.fetchImages();

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
        let cmd = this.s.cmd.value;

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

        let y = pearchive.packArchiveV1(this.inputEditor.getFiles());
        let z = pearchive.combineRequestAndArchive({
            'cmd': cmd.split(/\s+/),
        }, y);

        let req = new Request(window.location.origin + image.links.runi, {
            method: 'POST',
            body: z,
            headers: {
                'Content-type': 'application/x.pe.archivev1',
            }
        });
        const response = await fetch(req);
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

    async fetchImages() {
        let response = await fetch(window.location.origin + '/api/v1/images');
        if (response.ok) {
            let json: Api.Images.Response = await response.json();
            if (json.images?.length > 0) {
                let images: Map<string, Api.Image> = new Map(json.images.map(image => [image.info.digest, image]));
                this.s.images.value = images;
                this.s.selectedImage.value = images.keys().next()?.value;
            }
        } else {
            console.error(response);
        }
    }

    onImageSelect(event) {
        this.s.selectedImage.value = event.target.value;
    }

    onCmdChange(event) {
        this.s.cmd.value = event.target.value;
    }

    // this.props, this.state (all are signals)
    render() {
        let images = this.s.images.value;
        let selectedImage = this.s.selectedImage.value;
        let cmd = this.s.cmd.value;
        let lastStatus = this.s.lastStatus.value;

        // firefox supports Map().values().map(), but chrome doesn't
        let imageOptions = Array.from(images.values(), ({info,links}) => {
            let name = imageName(info);
            return <option key={info.digest} value={info.digest}>{name}</option>;
        });

        let imageDetails = null;
        let fullCommand = null;
        if (selectedImage !== null) {
            let image = images.get(selectedImage);
            imageDetails = (
                <details>
                    <summary>About this image</summary>
                    <a href={image.links.upstream} rel="nofollow">{imageName(image.info)}</a>
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
            fullCommand = JSON.stringify(computeFullCommand(image, cmd).cmd);
        }
        return (
            <div>
                <form>
                    <select onChange={e => this.onImageSelect(e)}>
                        {imageOptions}
                    </select>
                    <input className="mono" type="text" value={cmd} onChange={e => this.onCmdChange(e)} placeholder="env $entrypoint $cmd < /dev/null" />
                    <button className="mono" onClick={e => { e.preventDefault(); this.run(); }}>Run</button>
                    <span className="mono">{fullCommand}</span>
                    <span className="mono">{lastStatus ?? ''}</span>

                    {imageDetails}
                </form>

                <details>
                    <summary>Help</summary>
                    <p><kbd>Ctrl+Enter</kbd> within text editor will run</p>
                    <p>Input size limited to 1 MB</p>
                </details>

                <div id="editorSideBySide">
                    <Editor
                        ref={this.r_inputEditor}
                        ctrlEnterCb={(_editorView) => { this.run(); return true; }}
                        />
                    <Editor
                        ref={this.r_outputEditor}
                        readOnly={true} />
                </div>
        </div>
        );
    }
}

render(<App/>, document.getElementById('app'));
