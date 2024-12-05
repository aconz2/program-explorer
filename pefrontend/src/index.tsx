import { render, createRef, Component, RefObject } from 'preact';
//import { signal, Signal } from '@preact/signals';
import { EditorState } from '@codemirror/state';
import { EditorView, basicSetup } from 'codemirror';
import * as pearchive from './pearchive';

import './style.css';

enum FileKind {
    Editor,
    Blob,
}

type FileId = string;
type ImageId = string;

namespace Api {
    export namespace Runi {
        export type Request = {
            stdin?: string,
            entrypoint?: string[],
            cmd?: string[],
        }

        export type Response = {
            stdin?: string,
            entrypoint?: string[],
            cmd?: string[],
        }
    }

    export type Image = {
        links: {
            runi: string,
            upstream: string,
        },
        info: {
            digest: string,
            repository: string,
            registry: string,
            tag: string,
        },
        config: {
            created: string,
            architecture: string,
            os: string,
            config: {
                Cmd?: string[],
                Entrypoint?: string[],
                Env?: string[],
            },
            rootfs: {type: string, diff_ids: string[]}[],
            history: any, // todo
        },
    };
}

type AppState = {
    images: Map<ImageId, Api.Image>,
    selectedImage?: ImageId,
    cmd?: string,
}

class File {
    static _id = 0;

    id: string;
    path: string;
    kind: FileKind;
    editorState?: EditorState;
    data: string|ArrayBuffer;

    static _next_id(): number {
        File._id += 1;
        return File._id;
    }

    constructor(path, kind, data) {
        this.id = File._next_id().toString();
        this.path = path;
        this.kind = kind;
        this.data = data;
        if (this.kind == FileKind.Editor) {
            this.editorState = EditorState.create({
                doc: data,
            });
        }
    }

    static makeFile(path, data: string|ArrayBuffer): File {
        if (typeof data === 'string') {
            return new File(path, FileKind.Editor, data);
        } else {
            return new File(path, FileKind.Blob, data);
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

    constructor(files?: Map<FileId, File>, active?: FileId) {
        this.files = files ?? new Map();
        this.active = active ?? null;
    }

    static from(inputs: {path: string, data: string|ArrayBuffer}[]): FileStore {
        if (inputs.length === 0) return new FileStore();
        let files = new Map(inputs.map(({path,data}) => {
            let f = File.makeFile(path, data);
            return [f.id, f];
        }));
        let active = files.keys().next().value;
        return new FileStore(files, active);
    }

    addTextFile(path: string, data: string|ArrayBuffer): FileStore {
        let f = File.makeFile(path, data);
        let files = new Map(this.files);
        files.set(f.id, f);
        let active = this.active ?? f.id;
        return new FileStore(files, active);
    }

    addFiles(inputs: {path: string, data: string|ArrayBuffer}[]): FileStore {
        if (inputs.length === 0) return this;
        let fs = inputs.map(({path,data}) => File.makeFile(path, data));
        let files = new Map(this.files);
        for (let f of fs) {
            files.set(f.id, f);
        }
        let active = this.active ?? files.keys().next().value;
        return new FileStore(files, active);
    }

    setActive(file: File): FileStore {
        // do we have to copy this?
        this.active = file.id;
        return this;
    }
}

class Editor extends Component {
    ref = createRef();
    editor?: EditorView;
    props: {
        readOnly: boolean,
        placeholder?: string,
    };
    state: {
        store: FileStore,
    };

    constructor({readOnly, placeholder}: {readOnly?: boolean, placeholder?: string}) {
        super({placeholder,readOnly});
        this.state = {
            store: new FileStore(),
        };
    }

    componentDidMount() {
        this.editor = new EditorView({
          extensions: [
              basicSetup,
              ...(this.props.readOnly ? [EditorState.readOnly.of(true)] : []),
          ],
          parent: this.ref.current,
        });
    }

    addFile(path: string, data: string|ArrayBuffer) {
        let store = this.state.store.addFile(path, data);
        this.setState({store: store});
    }

    addFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = this.state.store.addFiles(files);
        this.setState({store: store});
    }

    setFiles(files: {path: string, data: string|ArrayBuffer}[]) {
        let store = FileStore.from(files);
        this.setState({store: store});
    }

    editFile(file: File) {
        this.setState({store: this.state.store.setActive(file)});
    }

    // this.props, this.state
    render({placeholder,readOnly}, {store}) {
        let tabs = Array.from(store.files.values(), file => {
            let className = 'tab mono';
            if (store.active == file.id) {
                className += ' selected';
            }
            return (
                <button
                    className={className}
                    key={file.id} onClick={() => this.editFile(file)}>
                    {file.displayName()}
                </button>
            );
        });
        if (this.editor && store.active) {
            let f = store.files.get(store.active);
            if (f.kind == FileKind.Editor) {
                this.editor.setState(f.editorState);
            }
        }
        return (
            <div class="editorContainer">
                {tabs}
                <div class="cmContainer" ref={this.ref}></div>
            </div>
        );
    }
}

class App extends Component {
    inputEditor: RefObject<Editor> = createRef();
    outputEditor: RefObject<Editor> = createRef();
    state: AppState = {
        images: new Map(),
        selectedImage: null,
        cmd: null,
    };

    componentDidMount() {
        // if you execute these back to back they don't both get applied...
        this.inputEditor.current.addFiles([
            {path:'test.sh', data:'echo "hello world"\ncat data.txt > output/data.txt'},
            {path:'blob', data: new Uint8Array([0, 0, 0, 0, 0])},
            //{path:'data.txt', data:'hi this is some data'},
            {path:'f1/dataf1.txt', data:'hi this is some data'},
            {path:'f1/f2/dataf1f2.txt', data:'hi this is some data'},
            {path:'f2/dataf2.txt', data:'hi this is some data'},
        ]);
        this.setState({cmd: 'sh test.sh'});

        this.fetchImages();

        setTimeout(() => {
            let y = pearchive.packArchiveV1(Array.from(this.inputEditor.current.state.store.files.values()));
            // only firefox has a Blob.bytes() method

            y.arrayBuffer().then(buf=>{
                let bytes = new Uint8Array(buf);
                console.log('----------------   packed -----------------------');
                console.log(bytes);
                console.log('---------------- unpacked (uint8array) -----------------------');
                console.log(pearchive.unpackArchiveV1(bytes));
                console.log('---------------- unpacked2 (arraybuffer) -----------------------');
                console.log(pearchive.unpackArchiveV1(buf));
                console.log('---------------- unpacked2 (dataview) -----------------------');
                console.log(pearchive.unpackArchiveV1(new DataView(buf, 62)));
            });
        }, 100);
    }

    async run(event) {
        event.preventDefault();

        let {images,selectedImage} = this.state;
        if (this.state.selectedImage === null) {
            console.warn('cant run without an image');
            return;
        }
        let image = images.get(selectedImage);

        let y = pearchive.packArchiveV1(Array.from(this.inputEditor.current.state.store.files.values()));
        let z = pearchive.combineRequestAndArchive({
            'cmd': ['sh', 'echo hi'],
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
        // todo unpack the archive
        let returnFiles = pearchive.unpackArchiveV1(archiveSlice);
        console.log(returnFiles);
        console.log(archiveSlice);

        this.outputEditor.current.setFiles(returnFiles);
    }

    async fetchImages() {
        let response = await fetch(window.location.origin + '/api/v1/images');
        if (response.ok) {
            let json = await response.json();
            if (json.images?.length > 0) {
                let images = new Map(json.images.map(image => [image.info.digest, image]));
                this.setState({
                    images: images,
                    selectedImage: images.keys().next().value,
                })
            }
        } else {
            console.error(response);
        }
    }

    onImageSelect(event) {
        this.setState({selectedImage: event.target.value});
    }

    onCmdChange(event) {
        this.setState({cmd: event.target.value});
    }

    // this.props, this.state
    render({}, {images,selectedImage,cmd}) {
        // firefox supports Map().values().map(), but chrome doesn't
        let imageOptions = Array.from(images.values(), ({info,links}) => {
            let name = imageName(info);
            return <option key={info.digest} value={links.runi}>{name}</option>;
        });

        let imageDetails = [];
        let fullCommand = '';
        if (selectedImage) {
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
        return <div>
            <form>
                <select onChange={e => this.onImageSelect(e)}>
                    {imageOptions}
                </select>
                <input className="mono" type="text" value={cmd} onChange={e => this.onCmdChange(e)} placeholder="env $entrypoint $cmd < /dev/null" />
                <button className="mono" onClick={e => this.run(e)}>Run</button>
                <span className="mono">{fullCommand}</span>

                {imageDetails}


            </form>

            <div id="editorSideBySide">
                <Editor ref={this.inputEditor} />
                <Editor
                    ref={this.outputEditor}
                    readOnly={true}
                    placeholder={PLACEHOLDER_DIRECTIONS} />
            </div>
        </div>
    }
}

const PLACEHOLDER_DIRECTIONS = `Run something to see the output...`;
// TODO something about the args going in not being compat with the Component type signature
render(<App/>, document.getElementById('app'));

//const response = await fetch(window.location.origin + '/api/v1/images');
//console.log(response);
//if (response.ok) {
//    const json = await response.json();
//    s_images.value = json;
//    if (json.images?.length > 0) {
//        s_chosenImage.value = json.images[0].links.runi;
//    }
//} else {
//    console.error(response);
//}
//
//<Editor
//  height="80vh"
//  theme="vs-dark"
//  path={file.name}
//  defaultLanguage={file.language}
//  defaultValue={file.value}
///>
//function getOrCeateFile(path, text) {
//    let uri = monaco.Uri.file(path);
//    return monaco.editor.getModel(uri) || monaco.editor.createModel(text, undefined, uri);
//}

