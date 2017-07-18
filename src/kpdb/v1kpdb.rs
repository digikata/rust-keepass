use std::cell::RefCell;
use std::rc::Rc;
use std::io::{Read, Write};
use std::fs::File;

use chrono::{DateTime, Local};
use rand;

use kpdb::GetIndex;
use kpdb::crypter::Crypter;
use kpdb::parser::{HeaderLoadParser, HeaderSaveParser, LoadParser, SaveParser};
use kpdb::v1error::V1KpdbError;
use kpdb::v1group::V1Group;
use kpdb::v1entry::V1Entry;
use kpdb::v1header::V1Header;
use super::super::sec_str::SecureString;

#[doc = "
V1Kpdb implements a KeePass v1.x database. Some notes on the file format:

* Database is encrypted with AES (Twofish currently not supported by this
  module) with a password and/or a keyfile.
* Database holds entries which describes the credentials (username, password
  URL...) and are sorted in groups
* The groups themselves can hold subgroups
* Entries have titles for better identification by the user and expiration
  dates to remind that the password should be changed after some period

TODO:

* saving
* editing
* use more pattern matching
* usage examples
* use mlock in proper places (editing)
"]
pub struct V1Kpdb {
    /// Filepath of the database
    pub path: String,
    /// Holds the header. Normally you don't need
    /// to manipulate this yourself
    pub header: V1Header,
    /// The groups which hold the entries
    pub groups: Vec<Rc<RefCell<V1Group>>>,
    /// The entries of the whole database
    pub entries: Vec<Rc<RefCell<V1Entry>>>,
    /// A group which holds all groups of level 0
    /// as a subgroup (all groups which are not a
    /// subgroup of another group )
    pub root_group: Rc<RefCell<V1Group>>,
    // Used to de- and encrypt the database
    crypter: Crypter,
}

impl V1Kpdb {
    /// Call this to create a new database instance. You have to call load
    /// to start decrypting and parsing of an existing database!
    /// path is the filepath of the database, password is the database password
    /// and keyfile is the filepath to the keyfile.
    /// password should already lie on the heap as a String type and not &str
    /// as it will be encrypted automatically and otherwise the plaintext
    /// would lie in the memory though
    pub fn new(path: String,
               password: Option<String>,
               keyfile: Option<String>)
               -> Result<V1Kpdb, V1KpdbError> {
        Ok(V1Kpdb {
            path: path,
            header: V1Header::new(),
            groups: vec![],
            entries: vec![],
            root_group: Rc::new(RefCell::new(V1Group::new())),
            crypter: try!(Crypter::new(password, keyfile)),
        })
    }

    /// Decrypt and parse the database.
    pub fn load(&mut self) -> Result<(), V1KpdbError> {
        let (header, encrypted_database) = try!(self.read_in_file());

        // First read header and decrypt the database
        let header_parser = HeaderLoadParser::new(header);
        self.header = try!(header_parser.parse_header());
        try!(self.check_header());
        let decrypted_database = try!(self.crypter
                                      .decrypt_database(&self.header, encrypted_database));

        // Next parse groups and entries.
        // pos is needed to remember position after group parsing
        let mut parser = LoadParser::new(decrypted_database,
                                         self.header.num_groups,
                                         self.header.num_entries);
        let (groups, levels) = try!(parser.parse_groups());
        self.groups = groups;
        self.entries = try!(parser.parse_entries());
        parser.delete_decrypted_content();

        // Now create the group tree and sort the entries to their groups
        try!(LoadParser::create_group_tree(self, levels));
        Ok(())
    }

    fn read_in_file(&self) -> Result<(Vec<u8>, Vec<u8>), V1KpdbError> {
        let mut file = try!(File::open(&self.path).map_err(|_| V1KpdbError::FileErr));
        let mut raw: Vec<u8> = vec![];
        try!(file.read_to_end(&mut raw).map_err(|_| V1KpdbError::ReadErr));
        let encrypted_database = raw.split_off(124);
        Ok((raw, encrypted_database))
    }

    fn check_header(&self) -> Result<(), V1KpdbError> {
        try!(self.header.check_signatures());
        try!(self.header.check_enc_flag());
        try!(self.header.check_version());
        Ok(())
    }
    
    pub fn save(&mut self,
                path: Option<String>,
                password: Option<String>,
                keyfile: Option<String>) -> Result<(), V1KpdbError> {
        let mut parser = SaveParser::new();
        parser.prepare(self);
        
        let mut header = self.header.clone();
        header.final_randomseed = (0..16).map(|_| rand::random::<u8>()).collect();
        header.iv = (0..16).map(|_| rand::random::<u8>()).collect();
        header.content_hash = try!(Crypter::get_content_hash(&parser.database));


        if let Some(new_password) = password {
            if new_password == "".to_string() {
                self.crypter.change_password(None);
            }
            else {
                self.crypter.change_password(Some(new_password));
            }
        }

        if let Some(new_keyfile) = keyfile {
            if new_keyfile == "".to_string() {
                self.crypter.change_keyfile(None);
            }
            else {
                self.crypter.change_keyfile(Some(new_keyfile));
            }
        }

        let encrypted_database = try!(self.crypter.encrypt_database(&header, parser.database));

        let mut header_parser = HeaderSaveParser::new(header);
        let header_raw = header_parser.parse_header();

        if let Some(new_path) = path {
            self.path = new_path
        }
        let mut file = try!(File::create(&self.path).map_err(|_| V1KpdbError::FileErr));
        try!(file.write_all(&header_raw).map_err(|_| V1KpdbError::WriteErr));
        try!(file.write_all(&encrypted_database).map_err(|_| V1KpdbError::WriteErr));
        try!(file.flush().map_err(|_| V1KpdbError::WriteErr));            

        Ok(())
    }
    
    /// Create a new group
    ///
    /// * title: title of the new group
    ///
    /// * expire: expiration date of the group
    ///           None means that the group expires never which itself
    ///           corresponds to the date 28-12-2999 23:59:59
    ///
    /// * image: an image number, used in KeePass and KeePassX for the group
    ///          icon. None means 0
    ///
    /// * parent: a group inside the groups vector which should be the parent in
    ///           the group tree. None means that the root group is the parent
    pub fn create_group(&mut self,
                        title: String,
                        expire: Option<DateTime<Local>>,
                        image: Option<u32>,
                        parent: Option<Rc<RefCell<V1Group>>>)
                        -> Result<(), V1KpdbError> {
        let mut new_id: u32 = 1;
        for group in self.groups.iter() {
            let id = group.borrow().id;
            if id >= new_id {
                new_id = id + 1;
            }
        }

        let new_group = Rc::new(RefCell::new(V1Group::new()));
        new_group.borrow_mut().id = new_id;
        new_group.borrow_mut().title = title;
        new_group.borrow_mut().creation = Local::now();
        new_group.borrow_mut().last_mod = Local::now();
        new_group.borrow_mut().last_access = Local::now();
        match expire {
            Some(s) => new_group.borrow_mut().expire = s,
            None => {} // is 12-28-2999 23:59:59 through V1Group::new
        }
        match image {
            Some(s) => new_group.borrow_mut().image = s,
            None => {} // is 0 through V1Group::new
        }
        match parent {
            Some(s) => {
                let index = try!(self.groups.get_index(&s));
                new_group.borrow_mut().parent = Some(s.clone());
                s.borrow_mut().children.push(Rc::downgrade(&new_group.clone()));
                self.groups.insert(index + 1, new_group);

            }
            None => {
                new_group.borrow_mut().parent = Some(self.root_group
                                                         .clone());
                self.root_group.borrow_mut().children.push(Rc::downgrade(&new_group.clone()));
                self.groups.push(new_group);
            }
        }

        self.header.num_groups += 1;
        Ok(())
    }

    /// Create a new entry
    ///
    /// * group: group which should hold the entry
    ///
    /// * title: title of the new entry
    ///
    /// * expire: expiration date of the group
    ///           None means that the group expires never which itself
    ///           corresponds to the date 28-12-2999 23:59:59
    ///
    /// * image: an image number, used in KeePass and KeePassX for the group
    ///          icon. None means 0
    ///
    /// * url: URL from where the credentials are
    ///
    /// * comment: some free-text-comment about the entry
    ///
    /// * username: username for the URL
    ///
    /// * password: password for the URL
    ///
    /// Note: username and password should be of type String at creation. If you have a
    /// &str which you convert into a String with to_string() the plaintext will remain
    /// in memory as the new created String is a copy of the original &str. If you use
    /// String this function call is a move so that the String remains where it was
    /// created.
    ///
    pub fn create_entry(&mut self,
                        group: Rc<RefCell<V1Group>>,
                        title: String,
                        expire: Option<DateTime<Local>>,
                        image: Option<u32>,
                        url: Option<String>,
                        comment: Option<String>,
                        username: Option<String>,
                        password: Option<String>) {
        // Automatically creates a UUID for the entry
        let new_entry = Rc::new(RefCell::new(V1Entry::new()));
        new_entry.borrow_mut().title = title;
        new_entry.borrow_mut().group = Some(group.clone());
        group.borrow_mut().entries.push(Rc::downgrade(&new_entry.clone()));
        new_entry.borrow_mut().group_id = group.borrow().id;
        new_entry.borrow_mut().creation = Local::now();
        new_entry.borrow_mut().last_mod = Local::now();
        new_entry.borrow_mut().last_access = Local::now();
        match expire {
            Some(s) => new_entry.borrow_mut().expire = s,
            None => {} // is 12-28-2999 23:59:59 through V1Entry::new()
        };
        match image {
            Some(s) => new_entry.borrow_mut().image = s,
            None => {} // is 0 through V1Entry::new()
        }
        new_entry.borrow_mut().url = url;
        new_entry.borrow_mut().comment = comment;
        match username {
            Some(s) => new_entry.borrow_mut().username = Some(SecureString::new(s)),
            None => {}
        };
        match password {
            Some(s) => new_entry.borrow_mut().password = Some(SecureString::new(s)),
            None => {}
        };

        self.entries.push(new_entry);
        self.header.num_entries += 1;
    }

    /// Remove a group
    ///
    /// * group: The group to remove
    ///
    /// Note: Entries and children of the group are deleted, too.
    ///
    /// The group should be given to the function as a move. If this is done, the rc counter
    /// is 0 at the end of the function and therefore sensitive data is deleted correctly.
    pub fn remove_group(&mut self, group: Rc<RefCell<V1Group>>) -> Result<(), V1KpdbError> {
        // Sensitive data (e.g. SecureString) is automatically dropped at the end of this
        // function as Rc is 0 then
        try!(self.remove_group_from_db(&group));
        try!(self.remove_entries(&group));
        if let Some(ref parent) = group.borrow().parent {
            try!(parent.borrow_mut().drop_weak_child_reference(&group));
            drop(parent);
        }
        try!(self.remove_children(&group));
        Ok(())
    }

    fn remove_group_from_db(&mut self, group: &Rc<RefCell<V1Group>>) -> Result<(), V1KpdbError> {
        let index = try!(self.groups.get_index(group));
        let db_reference = self.groups.remove(index);
        drop(db_reference);
        self.header.num_groups -= 1;
        Ok(())
    }

    fn remove_entry_from_db(&mut self, entry: &Rc<RefCell<V1Entry>>) -> Result<(), V1KpdbError> {
        let index = try!(self.entries.get_index(entry));
        let db_reference = self.entries.remove(index);
        drop(db_reference);
        self.header.num_entries -= 1;
        Ok(())
    }

    fn remove_entries(&mut self, group: &Rc<RefCell<V1Group>>) -> Result<(), V1KpdbError> {
        // Clone needed to prevent thread panning through borrowing
        let entries = group.borrow().entries.clone();
        for entry in entries {
            if let Some(entry_strong) = entry.upgrade() {
                try!(self.remove_entry(entry_strong));
            } else {
                return Err(V1KpdbError::WeakErr);
            }
        }
        Ok(())
    }

    fn remove_children(&mut self, group: &Rc<RefCell<V1Group>>) -> Result<(), V1KpdbError> {
        // Clone needed to prevent thread panning through borrowing
        let children = group.borrow().children.clone();
        for child in children {
            if let Some(child_strong) = child.upgrade() {
                try!(self.remove_group(child_strong));
            } else {
                return Err(V1KpdbError::WeakErr);
            }
        }
        Ok(())
    }

    /// Remove a group
    ///
    /// * entry: The entry to remove.
    ///
    /// Note: The entry should be given to the function as a move. If this is done, the rc counter
    /// is 0 at the end of the function and therefore sensitive data is deleted correctly.
    pub fn remove_entry(&mut self, entry: Rc<RefCell<V1Entry>>) -> Result<(), V1KpdbError> {
        // Sensitive data (e.g. SecureString) is automatically dropped at the end of this
        // function as Rc is 0 then
        try!(self.remove_entry_from_db(&entry));

        if let Some(ref group) = entry.borrow().group {
            try!(group.borrow_mut().drop_weak_entry_reference(&entry));
            drop(group);
        }
        Ok(())
    }
}
